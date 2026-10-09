import type { RunResult, UserInfo } from "./rustpad";

/** What the output panel shows. */
export type RunState = {
  status: "idle" | "running" | "finished" | "failed";
  by: UserInfo | null;
  result?: RunResult;
  error?: string;
};
