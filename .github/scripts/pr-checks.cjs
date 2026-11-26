// Checks every pull request goes through. pr-intake acts on them (closes,
// labels, comments); pr-review runs no model on a PR that fails one. Workflows
// load this file from the base checkout, never from the PR head; stale loads it
// for the team label only.
//
// - The contribution gate: a PR from outside the team needs a linked issue
//   that is a bug report or a feature request the maintainer marked `accepted`.
// - Visual evidence: screenshots belong in the PR description. Native view
//   changes need both Before and After screenshots.
//
// Each check's comment carries a marker and is posted once per PR. A PR with
// the comment but not the label had the label taken off by a maintainer, which
// waives that check for the PR for good; pr-intake retires the marker itself
// when a check passes, so it can apply again later.

const fs = require("node:fs");
const path = require("node:path");

const GATE_LABEL = {
  name: "needs-accepted-issue",
  color: "c5def5",
  description: "Closed: no linked bug report or accepted feature request. Reopens when one is linked.",
};

const GATE_MARKER = "<!-- pr-intake:needs-accepted-issue -->";

const TEAM_LABEL = {
  name: "team",
  color: "5319e7",
  description: "Opened by a maintainer. Never closed as stale.",
};

const COMMITTED_MEDIA = {
  label: {
    name: "evidence-in-branch",
    color: "e99695",
    description: "Screenshots, recordings or logs committed to the branch. They go in the PR description.",
  },
  // Not the marker of the older one-off evidence comment, which carried no
  // label and would otherwise read as a waiver.
  marker: "<!-- pr-intake:evidence-in-branch -->",
};

const MISSING_SCREENSHOTS = {
  label: {
    name: "needs-visual-evidence",
    color: "fbca04",
    description: "Required screenshots are missing from the PR description.",
  },
  marker: "<!-- pr-intake:needs-visual-evidence -->",
};

// PRs opened before the gate existed keep the rules they were opened under.
const GATE_CUTOVER = "2026-10-28T14:00:00Z";

const PASSING_ISSUE_LABELS = ["bug", "accepted"];

const isDocsFile = (file) => {
  if (file.startsWith("packages/docs/")) return true;
  if (!/\.mdx?$/i.test(file)) return false;
  // Agent instructions steer every contributor's agent; they are not docs.
  if (/^\.(agents|github|opencode)\//.test(file)) return false;
  return !/(^|\/)(AGENTS|CLAUDE)\.md$/.test(file);
};

const isTestFile = (file) =>
  /(^|\/)(__tests__|test|tests)\//.test(file) || /\.(test|spec)\.[^/]+$/.test(file) || /\.snap$/.test(file);

const readTrustedContributors = (workspace) => {
  try {
    return fs
      .readFileSync(path.join(workspace, ".github", "trusted-contributors"), "utf8")
      .split("\n")
      .map((line) => line.replace(/#.*/, "").trim().toLowerCase())
      .filter(Boolean);
  } catch (error) {
    if (error.code === "ENOENT") return [];
    throw error;
  }
};

// `author_association` reports MEMBER only when the caller can see the author's
// org membership, and an app token cannot see private members, so a maintainer
// with hidden membership reads as CONTRIBUTOR. The permission level is asked
// instead: it answers "read" for every GitHub user on a public repository, so
// Triage and up tells the team apart. The collaborator check is not used: the
// app token gets 404 there for maintainers whose access comes through the
// organization.
const hasRepoAccess = async ({ github, owner, repo, pr }) => {
  if (["OWNER", "MEMBER", "COLLABORATOR"].includes(pr.author_association)) return true;
  const { data } = await github.rest.repos.getCollaboratorPermissionLevel({ owner, repo, username: pr.user.login });
  return ["admin", "maintain", "write", "triage"].includes(data.role_name ?? data.permission);
};

// `role_name` tells maintain from write, which `permission` reports alike. An
// account that no longer exists has no role.
const isMaintainer = async ({ github, owner, repo, username }) => {
  try {
    const { data } = await github.rest.repos.getCollaboratorPermissionLevel({ owner, repo, username });
    return ["admin", "maintain"].includes(data.role_name);
  } catch (error) {
    if (error.status === 404) return false;
    throw error;
  }
};

// Issues the PR closes (`Fixes #N`, `Closes`, `Resolves`, or linked in the
// Development sidebar), plus `Part of #N` for one PR of a larger feature.
const linkedIssueNumbers = async ({ github, owner, repo, pr }) => {
  const { repository } = await github.graphql(
    `query($owner: String!, $repo: String!, $number: Int!) {
      repository(owner: $owner, name: $repo) {
        pullRequest(number: $number) {
          closingIssuesReferences(first: 25) { nodes { number repository { nameWithOwner } } }
        }
      }
    }`,
    { owner, repo, number: pr.number },
  );
  const numbers = new Set(
    repository.pullRequest.closingIssuesReferences.nodes
      .filter((issue) => issue.repository.nameWithOwner.toLowerCase() === `${owner}/${repo}`.toLowerCase())
      .map((issue) => issue.number),
  );
  const partOf = new RegExp(
    `\\bpart of\\s+(?:${owner}/${repo}#|#|https://github\\.com/${owner}/${repo}/issues/)(\\d+)`,
    "gi",
  );
  for (const match of (pr.body ?? "").matchAll(partOf)) numbers.add(Number(match[1]));
  return [...numbers];
};

const linkedIssues = async ({ github, owner, repo, pr }) => {
  const issues = [];
  for (const number of await linkedIssueNumbers({ github, owner, repo, pr })) {
    try {
      const { data } = await github.rest.issues.get({ owner, repo, issue_number: number });
      if (data.pull_request) continue;
      issues.push({ number, labels: data.labels.map((label) => label.name) });
    } catch (error) {
      if (error.status !== 404 && error.status !== 410) throw error;
    }
  }
  return issues;
};

// Returns { exempt } when the gate does not apply to this PR, otherwise
// { passes, issues } with the linked issues and their labels.
const evaluateContributionGate = async ({ github, owner, repo, pr, files, workspace }) => {
  if (pr.created_at < GATE_CUTOVER) return { exempt: "opened before the gate" };
  if (pr.draft) return { exempt: "draft" };
  if (pr.user.type === "Bot") return { exempt: "bot" };
  if (readTrustedContributors(workspace).includes(pr.user.login.toLowerCase())) return { exempt: "trusted contributor" };
  if (await hasRepoAccess({ github, owner, repo, pr })) return { exempt: "collaborator" };

  // A rename counts by both names, so moving code into a test path is not a test change.
  const paths = files.flatMap(({ filename, previous_filename }) => (previous_filename ? [filename, previous_filename] : [filename]));
  if (paths.length > 0 && paths.every((file) => isDocsFile(file) || isTestFile(file))) {
    return { exempt: "docs or tests only" };
  }

  const issues = await linkedIssues({ github, owner, repo, pr });
  const passes = issues.some((issue) => issue.labels.some((label) => PASSING_ISSUE_LABELS.includes(label)));
  return { passes, issues };
};

const listPullFiles = ({ github, owner, repo, pr }) =>
  github.paginate(github.rest.pulls.listFiles, { owner, repo, pull_number: pr.number, per_page: 100 });

// Media belongs only where the product's own assets live.
const ASSET_ROOTS = [
  "packages/ui/src/assets/",
  "packages/ui/public/",
  "packages/web/public/",
  "packages/electron/resources/",
  "packages/mobile/android/",
  "packages/mobile/ios/",
  "packages/mobile/assets/",
  "packages/vscode/",
  "packages/docs/",
  "packages/extensions/",
  "packages/sdk/examples/",
  "docs/references/",
];
const isMediaFile = (file) => /\.(png|jpe?g|gif|webp|bmp|mp4|mov|webm|har|trace|log)$/i.test(file);
const isEvidenceFile = (file) =>
  file.split("/").some((segment) => segment === "evidence" || segment === "pr-evidence") ||
  (isMediaFile(file) && !ASSET_ROOTS.some((root) => file.startsWith(root)));

// Rendered UI. All UI wording lives in the translation catalogs, so a wording
// change there needs no screenshot.
const isRenderedUiFile = (file) =>
  /^packages\/ui\/src\/.+\.(tsx|css)$/.test(file) && !isTestFile(file) && !file.startsWith("packages/ui/src/lib/i18n/messages/");

const isNativeViewFile = (file) =>
  /^native\/crates\/desktop\/src\/.*(?:_view|_settings|_page|_surface)(?:\.rs|\/.+\.rs)$/.test(file) &&
  !isTestFile(file) && !/(?:\/|_)tests?\.rs$/.test(file);

// A Markdown or HTML image, or a file GitHub hosts for the description.
const DESCRIPTION_IMAGE = /!\[[^\]]*\]\(\s*https?:\/\/|<img\s[^>]*src=["']?https?:\/\/|https:\/\/github\.com\/user-attachments\/assets\//i;

function markdownDestination(tail) {
  const close = /^(?:\s+(["'])(?:\\.|(?!\1)[^\\])*\1)?\s*\)/;
  if (tail.startsWith("<")) {
    const angle = /^<(https?:\/\/[^\s<>]+)>/i.exec(tail);
    if (angle && close.test(tail.slice(angle[0].length))) return angle[1];
    return;
  }
  if (!/^https?:\/\//i.test(tail)) return;
  let depth = 0;
  for (let i = 0; i < tail.length; i++) {
    if (tail[i] === "(") depth++;
    if (tail[i] === ")" && --depth < 0) return tail.slice(0, i);
    if (/\s/.test(tail[i])) {
      if (depth === 0 && close.test(tail.slice(i))) return tail.slice(0, i);
      return;
    }
  }
}

function markdownScreenshot(line) {
  const start = /^ {0,3}(!?)\[/.exec(line);
  if (!start) return;
  let depth = 1;
  for (let i = start[0].length; i < line.length; i++) {
    if (line[i] === "\\") {
      i++;
      continue;
    }
    if (line[i] === "[") depth++;
    if (line[i] !== "]" || --depth !== 0) continue;
    if (line[i + 1] !== "(") return;
    const destination = markdownDestination(line.slice(i + 2).trimStart());
    if (destination) return { destination, embedded: start[1] === "!" };
    return;
  }
}

function hasNativeScreenshotPair(body) {
  const images = { before: new Set(), after: new Set() };
  let section;
  let fence;
  for (const line of body.replace(/<!--[\s\S]*?(?:-->|$)/g, "").split(/\r?\n/)) {
    const marker = /^ {0,3}(`{3,}|~{3,})/.exec(line);
    if (fence) {
      if (marker && marker[1][0] === fence[0] && marker[1].length >= fence.length &&
          line.trim() === marker[1]) fence = undefined;
      continue;
    }
    if (marker) {
      fence = marker[1];
      continue;
    }
    const heading = /^ {0,3}#{1,6}\s+(.+?)\s*#*\s*$/.exec(line);
    if (heading) {
      const label = heading[1].toLowerCase();
      section = label === "before" || label === "after" ? label : undefined;
      continue;
    }
    if (!section) continue;
    const item = line.replace(/^ {0,3}(?:[-+*]|\d{1,9}[.)])\s+/, "");
    const markdown = markdownScreenshot(item);
    const html = /^ {0,3}<img(?=\s)[^>]*?\s+src=["'](https?:\/\/[^"']+)["'][^>]*>/i.exec(item);
    const destination = markdown?.destination ?? html?.[1];
    if (!destination) continue;
    const url = URL.parse(destination);
    if (!url || /\.(gif|mp4|mov|webm|m4v|avi)$/i.test(url.pathname)) continue;
    const attachment = url.hostname === "github.com" && /^\/user-attachments\/assets\/[^/]+$/.test(url.pathname);
    if (!markdown?.embedded && !html && !attachment && !/\.(png|jpe?g|webp|bmp)$/i.test(url.pathname)) continue;
    images[section].add(url.origin + url.pathname + url.search);
  }
  return [...images.before].some((before) => [...images.after].some((after) => before !== after));
}

const evaluateVisualEvidence = ({ pr, files }) => {
  const body = pr.body ?? "";
  const nativePairRequired = files.some(({ filename, previous_filename }) =>
    isNativeViewFile(filename) || (previous_filename && isNativeViewFile(previous_filename)));
  return {
    committedMedia: files
      .filter(({ status }) => status !== "removed")
      .map(({ filename }) => filename)
      .filter(isEvidenceFile),
    nativePairRequired,
    missingScreenshots: !pr.draft && (nativePairRequired
      ? !hasNativeScreenshotPair(body)
      : files.some(({ filename }) => isRenderedUiFile(filename)) && !DESCRIPTION_IMAGE.test(body)),
  };
};

const hasMarkerComment = (comments, marker) =>
  comments.some((comment) => comment.user?.type === "Bot" && (comment.body ?? "").includes(marker));

// Comments are read before the labels: pr-intake adds a label before it
// comments, so a check comment seen here comes with its label already set.
const isWaived = async ({ github, owner, repo, pr, label, marker }) => {
  const comments = await github.paginate(github.rest.issues.listComments, { owner, repo, issue_number: pr.number, per_page: 100 });
  if (!hasMarkerComment(comments, marker)) return false;
  const { data } = await github.rest.issues.get({ owner, repo, issue_number: pr.number });
  return !data.labels.some((item) => item.name === label.name);
};

module.exports = {
  COMMITTED_MEDIA,
  GATE_LABEL,
  GATE_MARKER,
  MISSING_SCREENSHOTS,
  TEAM_LABEL,
  evaluateContributionGate,
  evaluateVisualEvidence,
  hasMarkerComment,
  isMaintainer,
  isWaived,
  listPullFiles,
};
