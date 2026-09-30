import { describe, expect, it } from "vitest";
import {
  getIcon,
  getIconMetadata,
  hasIcon,
  iconList,
  isUrlIcon,
} from "@/icons/extracted";
import { registerForkIcons } from "@/icons/forkIcons";
import { providerPresets } from "@/config/claudeProviderPresets";

// Kiro 图标与 Command Code 一样由本发行版在 src/icons/forkIcons.ts 注册，
// extracted/ 的生成文件不再包含它，所以这里先跑一次注册。
registerForkIcons();

describe("Kiro provider icon", () => {
  it("resolves the preset icon to the fork-registered inline SVG", () => {
    const preset = providerPresets.find((item) => item.apiFormat === "kiro");
    expect(preset?.icon).toBe("kiro");
    expect(hasIcon("kiro")).toBe(true);
    expect(isUrlIcon("kiro")).toBe(false);
    expect(getIcon("kiro")).toContain('viewBox="0 0 1200 1200"');
    expect(getIcon("kiro")).toContain("#9046FF");
    expect(iconList).toContain("kiro");
    expect(getIconMetadata("kiro")?.displayName).toBe("Kiro");
  });
});
