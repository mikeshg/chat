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
