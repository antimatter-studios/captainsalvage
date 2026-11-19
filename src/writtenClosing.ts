/** Whether the "Something wrote to this disk" box is shown, kept apart from the window so it can be tested.
 *
 * The box is shown while the disk's structures differ from the baseline, until the person closes it. It
 * closes for one change, named by the hash of the structures now (`currentHash`): a different hash is a
 * new change and shows the box again, once; closing that one closes it again. When the disk is not
 * changed any more, the closing is forgotten, so the next change shows the box. The closing lasts as
 * long as the window does. What the box said stays one click away in the disk safety panel, which does
 * not depend on this. */
export function createClosedBox() {
  let closedFor: string | null = null;
  const listeners = new Set<() => void>();
  const tell = () => [...listeners].forEach((listener) => listener());
  return {
    /** The hash of the change that was closed, or null. */
    get: () => closedFor,
    /** Whether the box is shown for a disk that is (or is not) changed, with `key` as its current hash. */
    isOpen: (changed: boolean, key: string) => changed && closedFor !== key,
    close(key: string) {
      if (closedFor === key) return;
      closedFor = key;
      tell();
    },
    /** Call with whether the disk is changed: when it is not, a closing is forgotten. */
    settle(changed: boolean) {
      if (changed || closedFor === null) return;
      closedFor = null;
      tell();
    },
    subscribe(listener: () => void) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
  };
}

/** The one box of the window, shared by the box itself and anything that shows whether it is closed. */
export const closedBox = createClosedBox();
