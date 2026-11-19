# Local billing event queue

A managed gateway can buffer ordinary usage events across process restarts. Set
`managed.usage_spool_dir` to an absolute path on a writable persistent volume
dedicated to one gateway instance.

```yaml
managed:
  usage_spool_dir: /var/lib/aisix/usage
```

The gateway creates the queue directory with mode `0700` when its parent
exists and is writable. An existing directory must grant access only to its
owner. Use a filesystem that supports file locks, atomic rename and file and
directory synchronization. The gateway refuses to start if the directory is
already in use or belongs to another control plane or credential. Keep the
mount, control-plane URL, environment and gateway credential identity stable
across restarts. Protect the volume as billing data.

The chart exposes the same setting at `config.managed.usage_spool_dir`. Mount
persistent storage through `extraVolumes` and `extraVolumeMounts`. Each
replica needs its own storage. When a chart deployment uses one claim, run
one gateway replica with autoscaling disabled and set `updateStrategy.type`
to `Recreate`.

Enable the queue after every control-plane replica serving the configured
endpoint supports `X-Aisix-Usage-Batch-Dedup: 1`. Keep the in-memory sender
for mixed-version control planes that include an older receiver. On each
startup and after a failed connection, the gateway checks the authenticated
telemetry endpoint with a request that carries no batch ID or usage. A
capability check can only verify the replica that answers it.

The default is `null`, which uses the in-memory sender. With the local queue
enabled, request handlers still enqueue asynchronously. Events in memory and
an in-progress disk batch can be lost on an abrupt stop. The disk writer
starts a batch flush every 250 ms, or sooner when it fills a batch. A slow
disk or scheduling delays can extend the time before synchronization. A full
in-memory queue can still drop events. The request path never waits for
the local disk or the control plane.

After disk synchronization the gateway retries from the same stored batch
and event IDs. A control plane with the usage deduplication receipt removes
that batch from the local queue after confirming it. The existing 30-minute
delivery window is measured from the oldest event in each batch. The gateway
keeps expired, refused, partially accepted, and unconfirmed batches with the
`.reconcile` suffix. For reconciliation, use the batch UUID in the filename
to look up the control-plane batch and reject records. Archive reconciled
files out of the spool with the gateway stopped so it can recover their
capacity on restart.

Pending and reconciliation data share a limit of 1 GiB of request bodies and
16,384 batch files. Allow space for filesystem metadata and a temporary
batch. The gateway retains existing records at the limit and counts new
losses in `aisix_usage_event_drops_total` with reason `spool_full` or
`spool_write_failed`. Observe `aisix_usage_spool_batches` by state,
`aisix_usage_spool_bytes`, and `aisix_usage_spool_failures_total` by reason.

## Retire a gateway after draining

For a managed rolling deployment, stop and disable a replaced service after
traffic has moved and the old process's outstanding usage has been confirmed.
Use the exact credential and instance IDs from that process's receipts to
register retirement with `POST /admin/gateways/instances/{dp_id}/{instance_id}/retire`.
The control plane still considers a recently heard-from instance online, so
wait for its offline status before registering the retirement. A subsequent
heartbeat from the same instance clears its retirement.

The [systemd rollout helper](../scripts/gateway_rollout_retirement.py)
checks stop receipts, registers retirement through the admin API, and reads
back the current inventory before accepting a single-version rollout. It
checks for unretired older instances across the control plane's 24-hour
window and for incomplete inventory. Only use the helper for a rollout
whose target is one online gateway of the expected build.
