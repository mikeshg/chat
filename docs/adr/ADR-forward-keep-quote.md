# Forward keeps Reply Quote structure

Status: accepted (grill Q11 / Q1 product call, 2026-10-30)

Previously, forwarding deliberately stripped `reply_to` so the target conversation never carried the source reply pointer. Product now requires that a forward **keep** the Reply Quote structure so recipients still see what was being answered. When the quoted original is missing or not visible to the viewer, the client shows「原消息不可用」instead of dropping the quote chrome. This is a deliberate contract change (surprising relative to the old strip comment in forward handling) and is harder to reverse once clients and transcripts rely on preserved quotes—hence this ADR rather than a silent code tweak.
