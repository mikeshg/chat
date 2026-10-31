/** 会议房间号：12 位小写字母和数字。random 可注入，便于测试固定输出。 */
const ALPHABET = "abcdefghijklmnopqrstuvwxyz0123456789";

export function randomRoomId(): string {
  const bytes = new Uint8Array(12);
  crypto.getRandomValues(bytes);
  return [...bytes].map((b) => ALPHABET[b % ALPHABET.length]).join("");
}

/** 走现有文本发送链路的会议消息（遗留纯文本，kind=text）。base 末尾斜杠先去掉。 */
export function meetingMessage(base: string, random: () => string = randomRoomId): string {
  const root = base.replace(/\/+$/, "");
  return `📹 发起了视频会议，点击加入：${root}/chat-${random()}`;
}

/** 新 Meeting Card 用的绝对 URL（kind=meeting 的 content）。 */
export function meetingUrl(base: string, random: () => string = randomRoomId): string {
  const root = base.replace(/\/+$/, "");
  return `${root}/chat-${random()}`;
}

/** 从 kind=meeting 的 content 取出可打开的 URL；非法则 null。 */
export function parseMeetingUrl(content: string): string | null {
  const u = content.trim();
  if (u.startsWith("https://") || u.startsWith("http://")) return u;
  return null;
}
