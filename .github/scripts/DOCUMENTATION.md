# Pull request intake

## Ownership

`pr-checks.cjs` evaluates contribution and evidence requirements. The upstream
intake workflow owns contribution gating. The fork's `native-visual-evidence`
workflow applies the native screenshot label using the same evaluator.
`docs/rust-desktop/PARITY.md` owns the visual capture and review requirements.

## Invariants

- Privileged workflows load code from the trusted base. PR files are API data.
  They never check out or execute the PR head.
- Native view changes require distinct screenshots under `Before` and `After`
  headings. Parsing links does not authenticate captures or matched conditions.
- Drafts are exempt. Successful checks remove the label and retire the comment
  marker, so a later missing pair can be flagged again.
- Removing a label while its active bot comment remains is a maintainer waiver.
  Failed API reads propagate rather than clearing an evidence requirement.

## Decisions

### Why the fork has a separate evidence workflow

The upstream intake requires an upstream GitHub App and also manages contribution
gating and size labels. The fork checks native screenshot pairs with its scoped
workflow token, keeping the upstream automation unchanged.
