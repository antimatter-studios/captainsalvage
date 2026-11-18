import type { AgentChanges } from "./api";

/** What the bar over a Disk Rebuild offers for taking back what an agent did. */
export interface UndoBar {
  /** Show the bar at all: some agent change on this scan can still be undone. */
  show: boolean;
  /** What the latest change did, to name it on the Undo button. */
  summary: string | null;
  /** The Disk Rebuild showing was changed by an agent, so it can be restored to before. */
  canRestore: boolean;
  /** More than one agent change is waiting, so undoing all of a session means more than Undo. */
  several: boolean;
}

/**
 * Works out the undo bar from what agents changed on the scan and on the Disk Rebuild showing.
 * Both answers are the latest change not yet undone, and how many sessions have some left.
 */
export function undoBar(scan: AgentChanges | null, here: AgentChanges | null): UndoBar {
  const last = scan?.last ?? null;
  if (!last) return { show: false, summary: null, canRestore: false, several: false };
  const waiting = (scan?.sessions ?? []).reduce((total, session) => total + session.activeChanges, 0);
  return {
    show: true,
    summary: last.summary,
    canRestore: Boolean(here?.last),
    several: waiting > 1,
  };
}

/** The share of an agent session's allowance used, from 0 to 1, for saying how much is left. */
export function allowanceUsed(used: number, perSession: number): number {
  if (perSession <= 0) return 1;
  return Math.min(1, Math.max(0, used / perSession));
}
