import { describe, expect, it } from "vitest";
import {
  ADDITIVE_APP_IDS,
  APP_IDS,
  isAdditiveAppId,
  isProxyAppId,
  MCP_APP_IDS,
  orderAppIds,
  SKILLS_APP_IDS,
} from "@/config/appConfig";

describe("custom slim app set", () => {
  it("exposes only Claude Code, Claude Desktop, and Codex", () => {
    expect(APP_IDS).toEqual(["claude", "claude-desktop", "codex"]);
  });

  it("does not expose additive upstream apps", () => {
    expect(ADDITIVE_APP_IDS).toEqual([]);
    for (const appId of [
      "opencode",
      "openclaw",
      "hermes",
      "pi",
      "mcode",
      "kimi",
      "omp",
    ]) {
      expect(isAdditiveAppId(appId)).toBe(false);
    }
  });
});

// Antigravity shares management UI, but has independent configuration and no CCS proxy.
// DOVEGUO custom build: 界面只露出 claude / claude-desktop / codex，agy 不参与
// Skills / MCP 面板，也不走路由。
describe("Antigravity capabilities", () => {
  it("stays out of the skills, MCP and proxy app sets", () => {
    expect(SKILLS_APP_IDS).not.toContain("agy");
    expect(MCP_APP_IDS).not.toContain("agy");
    expect(isAdditiveAppId("agy")).toBe(false);
    expect(isProxyAppId("agy")).toBe(false);
  });
});

// 上游的用例依赖完整应用集（kimi / mcode 相邻等），在本发行版精简后不成立，
// 这里只保留在 claude / claude-desktop / codex 上仍然成立的行为。
describe("orderAppIds", () => {
  it("falls back to the default order when nothing is saved", () => {
    expect(orderAppIds(undefined)).toEqual(APP_IDS);
    expect(orderAppIds([])).toEqual(APP_IDS);
  });

  it("keeps the saved order and drops unknown or repeated ids", () => {
    const saved = [...APP_IDS].reverse();
    expect(orderAppIds(["future-app", ...saved, "claude"])).toEqual(saved);
  });

  it("appends ids missing from the saved order", () => {
    expect(orderAppIds(["codex", "claude"])).toEqual([
      "codex",
      "claude",
      "claude-desktop",
    ]);
  });
});
