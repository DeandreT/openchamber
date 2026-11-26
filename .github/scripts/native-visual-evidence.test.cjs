const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { test } = require("node:test");
const yaml = require("yaml");
const check = require("./native-visual-evidence.cjs");
const { MISSING_SCREENSHOTS } = require("./pr-checks.cjs");

const pair = [
  "### Before", "![Before](https://example.com/before.png)",
  "### After", "![After](https://example.com/after.png)",
].join("\n");

function fixture() {
  const calls = [];
  const pr = { number: 123, state: "open", draft: false, body: "", labels: [] };
  const files = [{ filename: "native/crates/desktop/src/appearance_view.rs", status: "modified" }];
  const comments = [];
  const issues = {
    listComments: async () => comments,
    getLabel: async () => { throw Object.assign(new Error("Absent"), { status: 404 }); },
    createLabel: async (value) => { calls.push(["createLabel", value]); },
    addLabels: async (value) => {
      calls.push(["addLabels", value]);
      pr.labels.push({ name: MISSING_SCREENSHOTS.label.name });
    },
    removeLabel: async (value) => {
      calls.push(["removeLabel", value]);
      pr.labels = [];
    },
    createComment: async (value) => {
      calls.push(["createComment", value]);
      comments.push({ id: comments.length + 1, user: { type: "Bot" }, body: value.body });
    },
    updateComment: async (value) => {
      calls.push(["updateComment", value]);
      comments.find(({ id }) => id === value.comment_id).body = value.body;
    },
  };
  const github = {
    rest: { issues, pulls: { get: async () => ({ data: pr }), listFiles: async () => files } },
    paginate: async (method, value) => method(value),
  };
  const context = { repo: { owner: "DeandreT", repo: "openchamber" }, payload: { pull_request: { number: 123 } } };
  const messages = [];
  const run = () => check({ github, context, core: { info: (message) => messages.push(message) } });
  return { calls, pr, files, comments, issues, github, context, messages, run };
}

test("the fork labels a missing pair once and clears it when the description supplies both", async () => {
  const f = fixture();
  await f.run();
  assert.deepEqual(f.calls.map(([name]) => name), ["createLabel", "addLabels", "createComment"]);
  assert.equal(f.calls[1][1].issue_number, 123);
  await f.run();
  assert.equal(f.calls.length, 3);
  f.pr.body = pair;
  await f.run();
  assert.deepEqual(f.calls.slice(3).map(([name]) => name), ["updateComment", "removeLabel"]);
  assert.ok(!f.comments[0].body.includes(MISSING_SCREENSHOTS.marker));
  f.pr.body = "";
  await f.run();
  assert.equal(f.comments.length, 2);
  assert.equal(f.pr.labels[0].name, "needs-visual-evidence");
});

test("drafts, nonvisual changes and closed PRs require no screenshots", async () => {
  for (const kind of ["draft", "nonvisual", "closed"]) {
    const f = fixture();
    if (kind === "draft") f.pr.draft = true;
    if (kind === "nonvisual") f.files[0].filename = ".github/scripts/pr-checks.cjs";
    if (kind === "closed") f.pr.state = "closed";
    await f.run();
    assert.deepEqual(f.calls, []);
  }
});

test("a maintainer waiver persists until a passing description retires its marker", async () => {
  const f = fixture();
  f.comments.push({ id: 1, user: { type: "Bot" }, body: MISSING_SCREENSHOTS.marker });
  await f.run();
  assert.deepEqual(f.calls, []);
  f.pr.body = pair;
  await f.run();
  assert.equal(f.calls[0][0], "updateComment");
  f.pr.body = "";
  await f.run();
  assert.equal(f.pr.labels.length, 1);
});

test("the comment event reads the current PR and the runner refuses another repository", async () => {
  const f = fixture();
  f.context.payload = { issue: { number: 123 } };
  f.pr.body = pair;
  await f.run();
  assert.match(f.messages[0], /native pair required=true; missing=false/);
  f.context.repo.owner = "another-owner";
  f.pr.body = "";
  await f.run();
  assert.deepEqual(f.calls, []);
});

test("API failures propagate without removing the label", async () => {
  for (const phase of ["files", "comments", "label"]) {
    const f = fixture();
    const fail = async () => { throw new Error("API unavailable"); };
    if (phase === "files") f.github.rest.pulls.listFiles = fail;
    if (phase === "comments") f.issues.listComments = fail;
    if (phase === "label") f.issues.getLabel = fail;
    await assert.rejects(f.run, /API unavailable/);
    assert.deepEqual(f.calls, []);
  }
});

test("a failed comment write can resume after the label was saved", async () => {
  const f = fixture();
  const createComment = f.issues.createComment;
  f.issues.createComment = async () => { throw new Error("Write failed"); };
  await assert.rejects(f.run, /Write failed/);
  assert.equal(f.pr.labels.length, 1);
  f.issues.createComment = createComment;
  await f.run();
  assert.equal(f.comments.length, 1);
  assert.equal(f.calls.filter(([name]) => name === "addLabels").length, 1);
});

test("label creation tolerates a concurrent creator and other failures stay errors", async () => {
  const f = fixture();
  f.issues.createLabel = async () => { throw Object.assign(new Error("Exists"), { status: 422 }); };
  await f.run();
  assert.equal(f.pr.labels.length, 1);
  const other = fixture();
  other.issues.createLabel = async () => { throw new Error("Denied"); };
  await assert.rejects(other.run, /Denied/);
  assert.deepEqual(other.pr.labels, []);
});

test("a failed marker retirement preserves the label and cannot create a waiver", async () => {
  const f = fixture();
  await f.run();
  f.pr.body = pair;
  const updateComment = f.issues.updateComment;
  f.issues.updateComment = async () => { throw new Error("Write failed"); };
  await assert.rejects(f.run, /Write failed/);
  assert.equal(f.pr.labels.length, 1);
  f.issues.updateComment = updateComment;
  await f.run();
  assert.equal(f.pr.labels.length, 0);
  f.pr.body = "";
  await f.run();
  assert.equal(f.pr.labels.length, 1);
});

test("the fork workflow uses trusted base code with a scoped token and no broad trigger", async () => {
  const workflow = yaml.parse(fs.readFileSync(path.join(__dirname, "../workflows/native-visual-evidence.yml"), "utf8"));
  assert.deepEqual(Object.keys(workflow.on), ["pull_request_target", "issue_comment"]);
  assert.deepEqual(workflow.on.pull_request_target.types, [
    "opened", "reopened", "synchronize", "ready_for_review", "converted_to_draft", "edited",
  ]);
  const job = workflow.jobs.evidence;
  assert.match(job.if, /github.repository == 'DeandreT\/openchamber'/);
  assert.match(job.if, /github.event.issue.pull_request/);
  assert.match(job.if, /github.event.comment.body == '\/recheck-native-evidence'/);
  assert.deepEqual(job.permissions, { contents: "read", issues: "write", "pull-requests": "read" });
  assert.equal(job["timeout-minutes"], 5);
  assert.equal(job.steps.length, 2);
  assert.match(job.steps[0].uses, /^actions\/checkout@[a-f0-9]{40}$/);
  assert.equal(job.steps[0].with.ref, "${{ github.event.pull_request.base.sha || github.event.repository.default_branch }}");
  assert.equal(job.steps[0].with["persist-credentials"], false);
  assert.match(job.steps[1].uses, /^actions\/github-script@[a-f0-9]{40}$/);
  const f = fixture();
  const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
  const script = new AsyncFunction("require", "github", "context", "core", job.steps[1].with.script);
  await script((requested) => {
    assert.equal(requested, `${process.env.GITHUB_WORKSPACE}/.github/scripts/native-visual-evidence.cjs`);
    return check;
  }, f.github, f.context, { info: (message) => f.messages.push(message) });
  assert.equal(f.pr.labels[0].name, "needs-visual-evidence");
});
