import { useEffect, useState } from "react";
import { auth } from "../../api";
import { storageText } from "../mediaFormat";
import SetRow from "./SetRow";

// ===== V2 P2-1 =====
// 「我」页存储卡片：本人附件已用空间与配额（0/缺省 = 不限额）。
export default function StorageUsage() {
  const [text, setText] = useState("加载中…");

  useEffect(() => {
    let live = true;
    auth
      .storage()
      .then((v) => {
        if (live) setText(storageText(v.used_bytes, v.quota_bytes));
      })
      .catch(() => {
        if (live) setText("加载失败");
      });
    return () => {
      live = false;
    };
  }, []);

  return (
    <SetRow label="存储空间">
      <span className="set-value">{text}</span>
    </SetRow>
  );
}
