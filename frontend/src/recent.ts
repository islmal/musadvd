export interface RecentProject { name: string; path: string; medium: string; modified: number }
export const RECENT_KEY = "musadvd-recent-projects";
const MAX = 10;

function defaultStore(): Storage | undefined {
  try {
    return window.localStorage;
  } catch {
    return undefined;
  }
}

export function loadRecent(store: Pick<Storage, "getItem"> | undefined = defaultStore()): RecentProject[] {
  try {
    const v = JSON.parse(store?.getItem(RECENT_KEY) ?? "[]");
    return Array.isArray(v) ? v.filter((e) => e && typeof e.path === "string" && typeof e.name === "string") : [];
  } catch {
    return [];
  }
}

function save(list: RecentProject[], store: Pick<Storage, "setItem"> | undefined): RecentProject[] {
  try {
    store?.setItem(RECENT_KEY, JSON.stringify(list));
  } catch {
    // storage unavailable: the list just is not remembered
  }
  return list;
}

const same = (a: string, b: string) => a.toLowerCase() === b.toLowerCase();

export function pushRecent(e: RecentProject, store: Pick<Storage, "getItem" | "setItem"> | undefined = defaultStore()): RecentProject[] {
  return save([e, ...loadRecent(store).filter((x) => !same(x.path, e.path))].slice(0, MAX), store);
}

export function removeRecent(path: string, store: Pick<Storage, "getItem" | "setItem"> | undefined = defaultStore()): RecentProject[] {
  return save(loadRecent(store).filter((x) => !same(x.path, path)), store);
}
