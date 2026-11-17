import { useEffect, useLayoutEffect, useState } from "react";
import type { TabProps } from "../App";
import { useHashQuery } from "../lib/useHashQuery";
import { callListHash } from "../lib/callLog";
import { CallLogDetails } from "../components/CallLogDetails";
import { Charges } from "./billing/Charges";

/** 请求由服务端合并；详情独立寻址，返回保留已加载页与局部搜索。 */
export function Calls(props: TabProps) {
  const [query] = useHashQuery();
  const eventId = query.get("detail");
  const [listVisited, setListVisited] = useState(!eventId);
  useEffect(() => { if (!eventId) setListVisited(true); }, [eventId]);
  useLayoutEffect(() => { window.scrollTo({ top: 0, left: 0, behavior: "instant" }); }, [eventId]);
  return <div className="measurement-records calls-workspace">
    {(listVisited || !eventId) && <div className="calls-list" hidden={Boolean(eventId)}><Charges {...props} /></div>}
    {eventId && <CallLogDetails eventId={eventId} backHref={callListHash(query)} />}
  </div>;
}
