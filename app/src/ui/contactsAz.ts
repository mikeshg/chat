import type { FriendView } from "../types";

export function sectionLetter(displayName: string): string {
  const t = displayName.trim();
  if (!t) return "#";
  const ch = t[0]!;
  if (/[A-Za-z]/.test(ch)) return ch.toUpperCase();
  const folded = ch.normalize("NFD").replace(/\p{M}/gu, "");
  if (/[A-Za-z]/.test(folded[0] ?? "")) return folded[0]!.toUpperCase();
  return "#";
}

export type FriendSection = { letter: string; friends: FriendView[] };

export function groupFriendsByLetter(friends: FriendView[]): FriendSection[] {
  const map = new Map<string, FriendView[]>();
  for (const f of friends) {
    const L = sectionLetter(f.user.display_name || f.user.username || "");
    const arr = map.get(L) ?? [];
    arr.push(f);
    map.set(L, arr);
  }
  const letters = [...map.keys()].sort((a, b) => {
    if (a === "#") return 1;
    if (b === "#") return -1;
    return a.localeCompare(b);
  });
  return letters.map((letter) => ({
    letter,
    friends: (map.get(letter) ?? []).slice().sort((a, b) =>
      (a.user.display_name || a.user.username).localeCompare(
        b.user.display_name || b.user.username,
        "zh",
      ),
    ),
  }));
}

export const AZ_INDEX = [
  ..."ABCDEFGHIJKLMNOPQRSTUVWXYZ".split(""),
  "#",
] as const;
