# Chat UI Redesign

Domain language for the chat client visual redesign epic and related message kinds (system bars, meeting cards, forward quotes). Implementation paths and CSS token names do not belong here.

## Language

**Accent**:
The primary brand color used for interactive highlights across light and dark themes (indigo light `#4F46E5`, dark `#6366F1`).
_Avoid_: Theme color, brand green, WeChat green, primary hex (when speaking in product language)

**System Message**:
A conversation event rendered as a centered gray bar for group lifecycle (create, rename, announce, invite, leave, dissolve), stored as message kind `system` with a machine-readable code—not as ordinary chat text.
_Avoid_: Notice, toast, announcement (when meaning lifecycle), status line, fake text prefix

**Meeting Card**:
A message of kind `meeting` shown as its own bubble with title/link affordances and a Join action that opens the configured meeting URL.
_Avoid_: Meeting link bubble, plain URL message, iframe embed

**Reply Quote**:
The nested reply/quote structure (`reply_to`) attached to a message, including when that message is forwarded; if the original is gone or invisible, the UI shows「原消息不可用」.
_Avoid_: Citation, embed, forward strip, orphan reply

**Slide-over Panel**:
A right-hand panel that slides over the chat (with a dimmed mask) instead of pushing the conversation layout aside.
_Avoid_: Sidebar squeeze, drawer that resizes chat, inline detail column

**In-app Modal**:
An application-owned confirmation/alert dialog with Chinese labels and automation-friendly controls, replacing native `window.confirm` / `alert` / `prompt`.
_Avoid_: Browser dialog, native confirm, English Cancel/OK box

**Bubble Group**:
Consecutive messages from the same sender within two minutes, visually clustered (shared avatar/spacing, unified 16px corner radius treatment).
_Avoid_: Thread, stack, batch send
