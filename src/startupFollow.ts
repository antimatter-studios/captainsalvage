/** Keeps the start screen's list of disks current without asking the disks more than needed.
 *
 * A look the person asked for (the screen opening, "Check again", the internal-disks switch) always
 * builds a report. A look that nobody asked for (a timer, the window coming back into focus) names
 * the fingerprint of the report on screen, and the engine answers `unchanged` unless the disks or
 * their mounts are different now: then nothing is rebuilt and nothing on screen moves. Two looks never
 * run at once, and an answer to a look that was overtaken is dropped. */

export interface Fingerprinted {
  fingerprint?: string;
}

export type Answer<R> = R | { unchanged: true };

export function isUnchanged<R>(answer: Answer<R>): answer is { unchanged: true } {
  return (answer as { unchanged?: boolean }).unchanged === true;
}

export type Look = "applied" | "unchanged" | "skipped" | "dropped";

export class DiskFollower<R extends Fingerprinted> {
  private fingerprint: string | null = null;
  private busy = false;
  private queued = false;
  private generation = 0;

  private readonly ask: (since: string | null) => Promise<Answer<R>>;
  private readonly apply: (report: R) => void;

  constructor(ask: (since: string | null) => Promise<Answer<R>>, apply: (report: R) => void) {
    this.ask = ask;
    this.apply = apply;
  }

  /** A look the person asked for: always builds a report, whatever else is running. */
  async look(): Promise<R> {
    const generation = (this.generation += 1);
    this.busy = true;
    try {
      const answer = await this.ask(null);
      if (isUnchanged(answer)) throw new Error("the disks answered `unchanged` to a look with no fingerprint");
      if (generation === this.generation) {
        this.fingerprint = answer.fingerprint ?? null;
        this.apply(answer);
      }
      return answer;
    } finally {
      this.finish(generation);
    }
  }

  /** A look nobody asked for: skipped when another is running, and a no-op when nothing changed. With
   * `queue`, a look that is skipped for another is made once that one is over, so a state read back
   * after a command is never lost behind a look that began before the command returned. */
  async refresh(options: { queue?: boolean } = {}): Promise<Look> {
    if (this.busy) {
      if (options.queue) this.queued = true;
      return "skipped";
    }
    const generation = this.generation;
    this.busy = true;
    try {
      // Without a fingerprint to compare (the screen was given a made-up list), ask in full.
      const answer = await this.ask(this.fingerprint);
      if (generation !== this.generation) return "dropped";
      if (isUnchanged(answer)) return "unchanged";
      this.fingerprint = answer.fingerprint ?? null;
      this.apply(answer);
      return "applied";
    } catch {
      // The list on screen stays as it was; the next look tries again.
      return "skipped";
    } finally {
      this.finish(generation);
    }
  }

  private finish(generation: number): void {
    if (generation !== this.generation) return;
    this.busy = false;
    if (this.queued) {
      this.queued = false;
      void this.refresh();
    }
  }
}
