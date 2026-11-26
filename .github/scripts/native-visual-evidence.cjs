const {
  MISSING_SCREENSHOTS,
  evaluateVisualEvidence,
  hasMarkerComment,
  listPullFiles,
} = require("./pr-checks.cjs");

module.exports = async ({ github, context, core }) => {
  if (`${context.repo.owner}/${context.repo.repo}` !== "DeandreT/openchamber") return;
  const number = context.payload.pull_request?.number ?? context.payload.issue?.number;
  if (!number) throw new Error("Missing pull request number");
  const target = { owner: context.repo.owner, repo: context.repo.repo, issue_number: number };
  const { data: pr } = await github.rest.pulls.get({
    owner: target.owner, repo: target.repo, pull_number: number,
  });
  if (pr.state !== "open") {
    core.info(`PR #${number} is ${pr.state}; no labels changed.`);
    return;
  }
  const files = await listPullFiles({ github, owner: target.owner, repo: target.repo, pr });
  const evidence = evaluateVisualEvidence({ pr, files });
  const failing = evidence.nativePairRequired && evidence.missingScreenshots;
  const { label, marker } = MISSING_SCREENSHOTS;
  const labeled = pr.labels.some(({ name }) => name === label.name);
  const comments = await github.paginate(github.rest.issues.listComments, { ...target, per_page: 100 });
  const commented = hasMarkerComment(comments, marker);
  if (failing && (labeled || !commented)) {
    if (!labeled) {
      try {
        await github.rest.issues.getLabel({ owner: target.owner, repo: target.repo, name: label.name });
      } catch (error) {
        if (error.status !== 404) throw error;
        try {
          await github.rest.issues.createLabel({ owner: target.owner, repo: target.repo, ...label });
        } catch (createError) {
          if (createError.status !== 422) throw createError;
        }
      }
      await github.rest.issues.addLabels({ ...target, labels: [label.name] });
    }
    if (!commented) await github.rest.issues.createComment({
      ...target,
      body: [
        marker,
        "This PR changes a Rust desktop view. Add screenshots from the PR base under a `Before` heading and from the final candidate under an `After` heading in the description.",
        "Match the capture conditions and identify each scenario, platform and revision. Keep Electron references separate. See `docs/rust-desktop/PARITY.md`, Visual pull request contract.",
        "Attach images in the description editor, or use `node scripts/pr-evidence.mjs`. A reviewer checks that the captures are authentic and matched.",
        "If this view change cannot affect rendered behavior, explain why so the maintainer can lift the label.",
      ].join("\n\n"),
    });
  } else if (!failing) {
    for (const comment of comments) {
      if (!hasMarkerComment([comment], marker)) continue;
      await github.rest.issues.updateComment({
        owner: target.owner, repo: target.repo, comment_id: comment.id,
        body: comment.body.replaceAll(marker, marker.replace(" -->", "-lifted -->")),
      });
    }
    if (labeled) {
      try {
        await github.rest.issues.removeLabel({ ...target, name: label.name });
      } catch (error) {
        if (error.status !== 404) throw error;
      }
    }
  }
  core.info(`PR #${number}: native pair required=${evidence.nativePairRequired}; missing=${failing}.`);
};
