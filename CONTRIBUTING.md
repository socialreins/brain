# Contributing to brain

brain is the API server of SocialReins. The contribution guide for both repos, with setup, the checks, pull request titles and how review works, is in the main repo:

**[socialreins/socialreins CONTRIBUTING.md](https://github.com/socialreins/socialreins/blob/main/CONTRIBUTING.md)**

The short version for brain:

```sh
docker compose up -d postgres
export DATABASE_URL=postgres://brain:brain@localhost:5432/socialreins_brain
export MAIL_LOG=true
export ENCRYPTION_KEY=$(openssl rand -base64 32)
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p socialreins-brain     # http://localhost:8080
```

Open an issue before anything bigger than a small fix. Every new or changed endpoint needs a request test in `app/tests/`. Security problems go to [SECURITY.md](https://github.com/socialreins/socialreins/blob/main/SECURITY.md), never an issue.

By contributing, you agree that your contributions are licensed under [AGPL-3.0](LICENSE), and you follow the [Code of Conduct](https://github.com/socialreins/socialreins/blob/main/CODE_OF_CONDUCT.md).
