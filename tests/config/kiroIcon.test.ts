import { describe, expect, it } from "vitest";
import {
  getIconMetadata,
  getIconUrl,
  hasIcon,
  iconList,
  isUrlIcon,
} from "@/icons/extracted";
import { providerPresets } from "@/config/claudeProviderPresets";

describe("Kiro provider icon", () => {
  it("resolves the preset icon to the bundled official SVG", () => {
    const preset = providerPresets.find((item) => item.apiFormat === "kiro");
    expect(preset?.icon).toBe("kiro");
    expect(hasIcon("kiro")).toBe(true);
    expect(isUrlIcon("kiro")).toBe(true);
    expect(getIconUrl("kiro")).toContain("kiro.svg");
    expect(iconList).toContain("kiro");
    expect(getIconMetadata("kiro")?.displayName).toBe("Kiro");
  });
});
