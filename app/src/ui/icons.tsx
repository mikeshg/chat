// 输入框工具栏的线性图标，描边随 currentColor
const svg = (d: string) => (
  <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor"
    strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
    <path d={d} />
  </svg>
);

export const PaperclipIcon = () =>
  svg("M21.44 11.05l-9.19 9.19a6 6 0 0 1-8.49-8.49l8.57-8.57A4 4 0 1 1 18 8.84l-8.59 8.57a2 2 0 0 1-2.83-2.83l8.49-8.48");
export const MicIcon = () =>
  svg("M12 2a3 3 0 0 0-3 3v7a3 3 0 0 0 6 0V5a3 3 0 0 0-3-3zM19 10v2a7 7 0 0 1-14 0v-2M12 19v3");
export const PollIcon = () => svg("M4 20h16M7 16V10M12 16V4M17 16v-7");

// ===== MM2：消息菜单 / 预览工具栏图标 =====
export const ReplyIcon = () => svg("M9 14 4 9l5-5M4 9h10.5a5.5 5.5 0 0 1 0 11H11");
export const CopyIcon = () =>
  svg("M9 9h11v11H9zM5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1");
export const ForwardIcon = () => svg("M15 17l5-5-5-5M20 12H9a5 5 0 0 0-5 5v3");
export const MultiSelectIcon = () =>
  svg("M9 11l3 3L22 4M21 12v7a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h11");
export const RecallIcon = () =>
  svg("M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8M3 3v5h5");
export const EyeIcon = () =>
  svg("M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7-10-7-10-7zM12 12m-3 0a3 3 0 1 0 6 0 3 3 0 1 0-6 0");
export const DownloadIcon = () =>
  svg("M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4M7 10l5 5 5-5M12 15V3");
// 下线的旧菜单项图标（feature flag 关闭，代码保留）
export const EmojiIcon = () =>
  svg("M12 22a10 10 0 1 1 0-20 10 10 0 0 1 0 20zM8 14s1.5 2 4 2 4-2 4-2M9 9h.01M15 9h.01");
export const EditIcon = () => svg("M17 3a2.83 2.83 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5Z");
export const PinIcon = () =>
  svg("M12 17v5M9 10.76a2 2 0 0 1-1.11 1.79l-1.78.9A2 2 0 0 0 5 15.24V16h14v-.76a2 2 0 0 0-1.11-1.79l-1.78-.9A2 2 0 0 1 15 10.76V5h1a2 2 0 0 0 0-4H8a2 2 0 0 0 0 4h1z");
export const StarIcon = () =>
  svg("M12 2l3.09 6.26L22 9.27l-5 4.87 1.18 6.88L12 17.77l-6.18 3.25L7 14.14 2 9.27l6.91-1.01z");

// 「我」页分类导航
export const UserIcon = () => svg("M20 21v-2a4 4 0 0 0-4-4H8a4 4 0 0 0-4 4v2M16 7a4 4 0 1 1-8 0 4 4 0 0 1 8 0z");
export const SlidersIcon = () =>
  svg("M4 21v-7M4 10V3M12 21v-9M12 8V3M20 21v-5M20 12V3M1 14h6M9 8h6M17 16h6");
export const BellIcon = () => svg("M18 8A6 6 0 0 0 6 8c0 7-3 9-3 9h18s-3-2-3-9M13.73 21a2 2 0 0 1-3.46 0");
export const BotIcon = () => svg("M12 8V4H8M4 8h16v12H4zM9 13v2M15 13v2");

// 后台管理分类导航
export const UsersIcon = () =>
  svg("M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2M13 7a4 4 0 1 1-8 0 4 4 0 0 1 8 0zM23 21v-2a4 4 0 0 0-3-3.87M16 3.13a4 4 0 0 1 0 7.75");
export const ServerIcon = () => svg("M2 3h20v7H2zM2 14h20v7H2zM6 6.5h.01M6 17.5h.01");
export const ArchiveIcon = () => svg("M21 8v13H3V8M1 3h22v5H1zM10 12h4");
