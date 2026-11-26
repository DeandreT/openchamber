const assert = require("node:assert/strict");
const { test } = require("node:test");
const { evaluateVisualEvidence } = require("./pr-checks.cjs");

const native = "native/crates/desktop/src/appearance_view.rs";
const web = "packages/ui/src/components/Appearance.tsx";
const before = "![Native before](https://example.com/before.png)";
const after = "![Native after](https://example.com/after.png)";
const pair = `### Before\n${before}\n\n### After\n${after}`;
const needsScreenshots = (body, filename = native, draft = false) =>
  evaluateVisualEvidence({ pr: { draft, body }, files: [{ filename, status: "modified" }] }).missingScreenshots;

test("a native view change needs Before and After images", () => {
  for (const body of ["", after, `### After\n${after}`, `### Before\n${before}\n\n### After`]) {
    assert.equal(needsScreenshots(body), true);
  }
  assert.equal(needsScreenshots(pair), false);
});

test("a native pair requires two distinct images outside comments and code", () => {
  for (const body of [
    `### Before\n${before}\n### After\n${before}`,
    `### Before\n${before}\n### After\n<!-- ${after} -->`,
    `### Before\n${before}\n### After\n\`\`\`md\n${after}\n\`\`\``,
    `### Before\n${before}\n### After\n#### Electron reference\n${after}`,
    `### Before\n${before}\n### After\n[Recording](https://example.com/after.webm)`,
    pair.replace(".png)", ".png"),
    pair.replace(/\.png/g, ".webm"),
    pair.replace("after.png", "before.png#other-label"),
  ]) {
    assert.equal(needsScreenshots(body), true);
  }
});

test("nested view paths and renamed view files still require a pair", () => {
  assert.equal(needsScreenshots("", "native/crates/desktop/src/settings/voice_view.rs"), true);
  assert.equal(needsScreenshots("", "native/crates/desktop/src/git_view/conflicts.rs"), true);
  assert.equal(needsScreenshots("", "native/crates/desktop/src/git_view/conflicts_tests.rs"), false);
  assert.equal(needsScreenshots("", "native/crates/desktop/src/git_view/tests.rs"), false);
  assert.equal(
    evaluateVisualEvidence({
      pr: { draft: false, body: "" },
      files: [{ filename: "native/crates/desktop/src/appearance.rs", previous_filename: native, status: "renamed" }],
    }).missingScreenshots,
    true,
  );
});

test("an attached image and a direct screenshot link can form the pair", () => {
  const body = [
    "## Before",
    '<img width="800" src="https://github.com/user-attachments/assets/demo" alt="Before">',
    "## After",
    "[After screenshot](https://example.com/after.webp)",
    "## Electron reference",
    "![Electron](https://example.com/reference.png)",
  ].join("\r\n");
  assert.equal(needsScreenshots(body), false);
});

test("image lists and resource IDs in URLs are supported", () => {
  const body = [
    "## Before", "- ![Before](https://example.com/image?id=100)",
    "## After", "1. ![After](https://example.com/image?id=101)",
  ].join("\n");
  assert.equal(needsScreenshots(body), false);
});

test("uploader URLs with balanced parentheses and direct GitHub attachments work", () => {
  const gist = "https://gist.githubusercontent.com/user/id/raw/revision/";
  const uploads = [
    "## Before", `![Before](${gist}settings%20(dark)-before.png)`,
    "## After", `![After](${gist}settings%20(dark)-after.png)`,
  ].join("\n");
  assert.equal(needsScreenshots(uploads), false);
  assert.equal(needsScreenshots(uploads.replace("before.png)", "before.png")), true);
  const base = "https://github.com/user-attachments/assets/";
  assert.equal(needsScreenshots(`## Before\n[Before](${base}first)\n## After\n[After](${base}second)`), false);
});

test("balanced or escaped labels and matching Markdown titles work", () => {
  const prefix = `## Before\n${before}\n## After\n`;
  assert.equal(needsScreenshots(prefix + `![Settings [dark]](<https://example.com/after.png> "What's new")`), false);
  assert.equal(needsScreenshots(prefix + `![Settings \\[dark\\]](https://example.com/after.png 'The "After" view')`), false);
  assert.equal(needsScreenshots(prefix + `![Settings](https://example.com/after.png "Bad title')`), true);
  assert.equal(needsScreenshots(prefix + `![Settings](<https://example.com/after.png> "Bad title')`), true);
  assert.equal(needsScreenshots(prefix + `![Settings](<https://example.com/after.png>"Bad title")`), true);
});

test("non-native evidence checks and draft exemptions stay the same", () => {
  assert.equal(needsScreenshots("", web), true);
  assert.equal(needsScreenshots(after, web), false);
  assert.equal(needsScreenshots("", native, true), false);
  assert.equal(needsScreenshots("", "native/crates/desktop/src/host_manager.rs"), false);
  assert.equal(needsScreenshots("", "native/crates/desktop/src/tests/appearance_view.rs"), false);
});
