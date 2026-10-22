import { describe, expect, it } from "vitest";
import {
  ADDITIVE_APP_IDS,
  APP_IDS,
  isAdditiveAppId,
  isProxyAppId,
  MCP_APP_IDS,
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
