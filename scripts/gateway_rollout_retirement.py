"""单版本网关滚动发布的最后一道验收。调用方负责切流、排空和停机证明。"""

import json
import re
import subprocess
import time
from typing import NamedTuple
from uuid import UUID


INSTANCE_PATH = "/admin/gateways/instances"

# 与控制面 compat::gate 的窗口和实例上限一致。在同一只读快照中检查，
# 避免分页、客户端时钟差或不同查询时刻掩盖不完整的清单。
FLEET_SQL = """
SELECT json_build_object(
    'recent_unretired', COALESCE((
        SELECT json_agg(json_build_object(
            'dp_id', dp_id, 'instance_id', instance_id, 'version', version
        ) ORDER BY dp_id, instance_id)
        FROM dp_instances
        WHERE last_seen_at > now() - interval '24 hours' AND retired_at IS NULL
    ), '[]'::json),
    'recent_cap_hit', EXISTS (
        SELECT 1 FROM dp_credentials WHERE instance_cap_hit_at > now() - interval '24 hours'
    ),
    'full_inventory', EXISTS (
        SELECT 1 FROM dp_instances GROUP BY dp_id HAVING count(*) >= 128
    )
)
"""


class Retirement(NamedTuple):
    unit: str
    dp_id: str
    instance_id: str
    version: str


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def stopped_instance(release, role, unit, version, boot_id):
    def receipt(name):
        path = release / f"{name}-{role}.json"
        require(path.is_file(), f"missing retirement receipt: {path.name}")
        return json.loads(path.read_text())

    drained = receipt("drained")["payload"]
    exited = receipt("exited")
    ack = receipt("ack-retired")
    ready = drained["ready"]
    process = ready["identity"]["pid"]
    require(ready == exited["ready"]
            and drained["unit"] == exited["unit"] == ack["unit"] == unit
            and drained["boot_id"] == exited["boot_id"] == ack["boot_id"] == boot_id
            and type(process) is int and process > 0 and ack["PID"] == process
            and ack["after_stop"] is True and ack["no_drop_or_reject_in_boot"] is True
            and re.fullmatch(r"[0-9a-f]{64}", ack["proof_sha256"] or ""),
            f"unbound or incomplete retirement receipt: {unit}")
    return Retirement(unit, str(UUID(ready["dp_id"])), str(UUID(ready["instance_id"])), version)


def systemd_stopped(target):
    expected = {
        "ActiveState": "inactive", "UnitFileState": "disabled", "MainPID": "0",
        "Result": "success", "ExecMainStatus": "0",
    }
    result = subprocess.run(
        ["systemctl", "show", target.unit, *[f"--property={name}" for name in expected]],
        check=True, capture_output=True, text=True, timeout=10,
    )
    return dict(line.split("=", 1) for line in result.stdout.splitlines()) == expected


def instance_key(row):
    return row["dp_id"], row["instance_id"]


def read(request, path):
    status, result = request("GET", path)
    require(status == 200, f"control plane GET {path} returned HTTP {status}")
    return result


def inventory(request, final_instance, version, retiring):
    items = read(request, INSTANCE_PATH)["items"]
    rows = {instance_key(row): row for row in items}
    require(len(rows) == len(items), "duplicate gateway instances in control plane inventory")
    final = rows.get(final_instance)
    require(final and final["online"] is True and final["version"] == version and not final["retired_at"],
            "the new gateway is not online on the expected version")
    require(all(identity == final_instance or identity in retiring or row["online"] is False
                for identity, row in rows.items()), "unexpected online gateway")
    return rows


def stopped_row(target, rows, verify_stopped):
    require(verify_stopped(target) is True, f"service no longer proven stopped and disabled: {target.unit}")
    row = rows.get((target.dp_id, target.instance_id))
    require(row and row["version"] == target.version,
            f"original gateway instance absent or version changed: {target.unit} ({target.instance_id})")
    return row


def complete_rollout(*, version, final_instance, retirements, request, verify_stopped,
                     fleet_status, check_only=False, timeout=120):
    """校验并登记确切进程的退役，最后读回控制面的完整版本窗口。"""
    retiring = {(target.dp_id, target.instance_id): target for target in retirements}
    require(len(retiring) == len(retirements) and final_instance not in retiring,
            "duplicate or current gateway in retirement plan")
    require(read(request, "/admin/environment")["cp_version"] == version, "unexpected control plane version")
    deadline = time.monotonic() + timeout

    rows = inventory(request, final_instance, version, retiring)
    for identity, target in retiring.items():
        while True:
            row = stopped_row(target, rows, verify_stopped)
            if row["online"] is False:
                if row["retired_at"]:
                    break
                require(not check_only, f"stopped gateway remains unretired: {target.unit} ({target.instance_id})")
                path = f"{INSTANCE_PATH}/{target.dp_id}/{target.instance_id}/retire"
                status, result = request("POST", path, {"confirmed_stopped": True})
                if status == 200:
                    require(instance_key(result) == identity and result["retired_at"],
                            f"retirement response does not match {target.unit}")
                    break
                require(status == 409 and result.get("code") == "gateway_instance_online",
                        f"retirement API returned HTTP {status} for {target.unit}")
            require(time.monotonic() < deadline, f"gateway still online in control plane: {target.unit}")
            time.sleep(1)
            rows = inventory(request, final_instance, version, retiring)

    rows = inventory(request, final_instance, version, retiring)
    for target in retirements:
        row = stopped_row(target, rows, verify_stopped)
        require(row["online"] is False and row["retired_at"], f"retirement did not persist: {target.unit}")

    fleet = fleet_status()
    require(fleet["recent_cap_hit"] is False and fleet["full_inventory"] is False,
            "gateway inventory is incomplete or at its instance limit")
    unretired = {instance_key(row): row["version"] for row in fleet["recent_unretired"]}
    blockers = [f"{identity[0]}/{identity[1]} ({found_version})"
                for identity, found_version in unretired.items()
                if found_version != version or identity in retiring]
    require(not blockers, "unretired gateway instances block release: " + ", ".join(blockers))
    require(unretired.get(final_instance) == version, "the new gateway is missing from the recent fleet")
    return {"retired_instances": len(retirements), "recent_unretired_instances": len(unretired),
            "version": version}
