"""发布结束必须以控制面接受并保留的实例记录为准。"""

import json
from pathlib import Path
import tempfile
import unittest

from gateway_rollout_retirement import Retirement, complete_rollout, stopped_instance


DP = "00000000-0000-4000-8000-000000000001"
OLD = "00000000-0000-4000-8000-000000000002"
FINAL = "00000000-0000-4000-8000-000000000003"
VERSION = "dev+sha-current"
RETIRED_AT = "2026-11-18T12:00:00Z"


class RolloutTest(unittest.TestCase):
    def setUp(self):
        self.stopped = True
        self.posts = []
        self.persist_retirement = True
        self.capped = False
        self.full = False
        self.rows = [
            dict(dp_id=DP, instance_id=OLD, version="dev+sha-old", online=False, retired_at=None),
            dict(dp_id=DP, instance_id=FINAL, version=VERSION, online=True, retired_at=None),
        ]
        self.target = Retirement("old.service", DP, OLD, "dev+sha-old")

    def request(self, method, path, body=None):
        if path == "/admin/environment":
            return 200, {"cp_version": VERSION}
        if method == "GET" and path == "/admin/gateways/instances":
            return 200, {"items": [row.copy() for row in self.rows]}
        self.posts.append((method, path, body))
        row = next(row for row in self.rows
                   if path == f"/admin/gateways/instances/{DP}/{row['instance_id']}/retire")
        if self.persist_retirement:
            row["retired_at"] = RETIRED_AT
        return 200, {"dp_id": DP, "instance_id": row["instance_id"], "retired_at": RETIRED_AT}

    def fleet(self):
        return {
            "recent_unretired": [row for row in self.rows if not row["retired_at"]],
            "recent_cap_hit": self.capped,
            "full_inventory": self.full,
        }

    def complete(self, retirements=None, check_only=False):
        return complete_rollout(
            version=VERSION, final_instance=(DP, FINAL),
            retirements=[self.target] if retirements is None else retirements,
            request=self.request, verify_stopped=lambda _: self.stopped,
            fleet_status=self.fleet, check_only=check_only, timeout=0,
        )

    def test_registers_retirement_before_success_and_can_retry(self):
        self.complete()
        self.complete()
        self.assertEqual(self.rows[0]["retired_at"], RETIRED_AT)
        self.assertEqual(self.posts, [
            ("POST", f"/admin/gateways/instances/{DP}/{OLD}/retire", {"confirmed_stopped": True}),
        ])

    def test_refuses_to_retire_an_unproven_stop(self):
        self.stopped = False
        with self.assertRaisesRegex(RuntimeError, "stopped and disabled"):
            self.complete()
        self.assertEqual(self.posts, [])

    def test_waits_for_control_plane_to_confirm_offline(self):
        self.rows[0]["online"] = True
        with self.assertRaisesRegex(RuntimeError, "still online"):
            self.complete()
        self.assertEqual(self.posts, [])

    def test_readback_required_after_successful_post(self):
        self.persist_retirement = False
        with self.assertRaisesRegex(RuntimeError, "retirement did not persist"):
            self.complete()

    def test_unlisted_old_gateway_blocks_release(self):
        with self.assertRaisesRegex(RuntimeError, "unretired"):
            self.complete(retirements=[])
        self.assertEqual(self.posts, [])

    def test_fleet_cannot_pass_when_inventory_is_incomplete(self):
        self.rows[0]["retired_at"] = RETIRED_AT
        for field in ("capped", "full"):
            setattr(self, field, True)
            with self.assertRaisesRegex(RuntimeError, "inventory"):
                self.complete()
            setattr(self, field, False)
        self.assertEqual(self.posts, [])

    def test_check_only_never_writes_retirement(self):
        with self.assertRaisesRegex(RuntimeError, "unretired"):
            self.complete(check_only=True)
        self.assertIsNone(self.rows[0]["retired_at"])
        self.assertEqual(self.posts, [])

    def test_different_gateway_with_same_credential_is_not_retired(self):
        self.rows[0]["instance_id"] = "00000000-0000-4000-8000-000000000004"
        with self.assertRaisesRegex(RuntimeError, "original gateway instance absent"):
            self.complete()
        self.assertEqual(self.posts, [])

    def test_late_heartbeat_after_api_readback_still_fails(self):
        self.rows[0]["retired_at"] = RETIRED_AT
        # 临时网关可能与最终网关同版本，仍须验证已退役的实例没有恢复。
        self.target = self.target._replace(version=VERSION)
        self.rows[0]["version"] = VERSION
        fleet = self.fleet()
        fleet["recent_unretired"].append(self.rows[0])
        self.fleet = lambda: fleet
        with self.assertRaisesRegex(RuntimeError, "unretired"):
            self.complete()


class StopReceiptTest(unittest.TestCase):
    def test_puts_only_the_acknowledged_process_in_the_retirement_plan(self):
        with tempfile.TemporaryDirectory() as tmp:
            release = Path(tmp)
            boot = "current-boot"
            scope = dict(unit="old.service", boot_id=boot)
            ready = dict(dp_id=DP, instance_id=OLD, identity={"pid": 117})
            for name, data in (
                ("drained-old", dict(payload=dict(ready=ready, **scope))),
                ("exited-old", dict(ready=ready, **scope)),
                ("ack-retired-old", dict(**scope, PID=117, after_stop=True,
                                        no_drop_or_reject_in_boot=True, proof_sha256="a" * 64)),
            ):
                (release / (name + ".json")).write_text(json.dumps(data))
            args = (release, "old", "old.service", "dev+sha-old", boot)
            self.assertEqual(stopped_instance(*args), Retirement("old.service", DP, OLD, "dev+sha-old"))
            ack_path = release / "ack-retired-old.json"
            ack = json.loads(ack_path.read_text())
            for changed in ({"PID": 118}, {"boot_id": "other-boot"}, {"after_stop": False},
                            {"no_drop_or_reject_in_boot": False}, {"proof_sha256": None}):
                ack_path.write_text(json.dumps(ack | changed))
                with self.assertRaisesRegex(RuntimeError, "unbound or incomplete"):
                    stopped_instance(*args)


if __name__ == "__main__":
    unittest.main()
