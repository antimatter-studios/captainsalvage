import { createContext, useContext } from "react";

/** Simple shows what recovering files needs; Investigate adds the map, bytes and raw tools. */
export type Mode = "simple" | "investigate";

export const ModeContext = createContext<Mode>("investigate");

/** Whether the window is in Simple mode, where no cluster number is ever shown. */
export function useSimple(): boolean {
  return useContext(ModeContext) === "simple";
}
