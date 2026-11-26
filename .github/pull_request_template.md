## What changed and why

<!-- One change per pull request. Link the issue: "Closes #123". -->

## How I tested it

<!-- Commands you ran, requests you made. -->

## Checklist

- [ ] `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` pass
- [ ] New or changed endpoints have a request test in `app/tests/`
- [ ] No secrets, no em dashes
- [ ] The README's endpoint list is updated if the API changed
- [ ] A new migration: take a database backup just before merging, and check it works while the old brain still runs (a deploy overlaps)
