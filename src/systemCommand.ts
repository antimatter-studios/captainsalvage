/** What the window does after the app has sent the system a command that can change the disks: an
 * unmount, a change to which volumes macOS may mount, and anything else that reaches `diskutil` or
 * the like. The command's result is not trusted to be the new state: every screen that shows disks reads
 * the state back, once when the command returns and once more shortly after, because the system's
 * list of mounts can lag behind a command that has returned. Whoever shows disks calls `onSystemCommand`. */

/** How long after a command returns the state is read a second time. */
export const AFTER_COMMAND_MILLIS = 1500;

const listeners = new Set<() => void>();

/** Calls `listener` after every system command, now and again `AFTER_COMMAND_MILLIS` later. Returns
 * what stops it. */
export function onSystemCommand(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** Says that a system command has just returned (or failed, which may have changed things too). */
export function announceSystemCommand(): void {
  const tell = () => [...listeners].forEach((listener) => listener());
  tell();
  setTimeout(tell, AFTER_COMMAND_MILLIS);
}

/** Runs one system command, and announces it whatever the result. */
export async function afterSystemCommand<T>(run: () => Promise<T>): Promise<T> {
  try {
    return await run();
  } finally {
    announceSystemCommand();
  }
}
