/** 会议房间号：12 位小写字母和数字。random 可注入，便于测试固定输出。 */
const ALPHABET = "abcdefghijklmnopqrstuvwxyz0123456789";

export function randomRoomId(): string {
  const bytes = new Uint8Array(12);
  crypto.getRandomValues(bytes);
  return [...bytes].map((b) => ALPHABET[b % ALPHABET.length]).join("");
}

/** 走现有文本发送链路的会议消息。base 末尾斜杠先去掉，避免出现双斜杠。 */
export function meetingMessage(base: string, random: () => string = randomRoomId): string {
  const root = base.replace(/\/+$/, "");
  return `📹 发起了视频会议，点击加入：${root}/chat-${random()}`;
}
