import { useState } from "react";
import { ApiError } from "../lib/api";
import { formMessageFor } from "../lib/errors";
import { Button } from "../ui/Button";
import { useToast } from "../ui/Toast";

export function CsvExportButton({ label = "下载 CSV", disabled, onExport }: {
  label?: string;
  disabled?: boolean;
  onExport: () => Promise<void>;
}) {
  const [busy, setBusy] = useState(false);
  const toast = useToast();

  async function download() {
    setBusy(true);
    try {
      await onExport();
    } catch (error) {
      toast(error instanceof ApiError ? formMessageFor(error) : "无法下载 CSV，请稍后重试");
    } finally {
      setBusy(false);
    }
  }

  return <Button busy={busy} busyLabel="正在下载 CSV" disabled={disabled} onClick={() => void download()}>{label}</Button>;
}
