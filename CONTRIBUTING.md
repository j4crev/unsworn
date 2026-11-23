# Contributing to Unsworn

Thanks for wanting to help. Unsworn takes outside contributions, but every
change starts with a conversation, not a pull request.

## Start with an issue or a discussion

- **Found a bug?** Open an [issue](https://github.com/j4crev/unsworn/issues).
  Say what you ran, what you expected and what happened instead. The smallest
  `unsworn.toml` and request that show it help most.
- **Have an idea, a feature or a bigger change?** Start a
  [discussion](https://github.com/j4crev/unsworn/discussions). Say what problem
  it solves and how you'd approach it.
- **Found a security problem?** Don't post it publicly. Follow
  [SECURITY.md](SECURITY.md).

Wait for a maintainer to agree before you write code. Some ideas don't fit
Unsworn, and some bugs already have a fix on the way. A short conversation
first saves you work that wouldn't be merged.

## Then open a pull request

**Pull requests without a linked issue or discussion are closed without
review.** In the pull request's description:

- link the issue (`Fixes #123`) or the discussion it implements
- say what changed and why
- note anything a reviewer should look at closely, such as a change to how
  a rule is enforced

Keep one pull request to one change. Small, focused pull requests are
reviewed sooner.

## AI-assisted contributions

You can use AI tools, but you're responsible for what you submit. Ensure
all checks pass, and be ready to answer questions about the change. The
rules above apply the same way: one agreed issue or discussion, then one
pull request.

## Working on the code

The repository is a Rust workspace with handler SDKs in `sdk/`. The full
test suite runs every SDK's worker against the binary, so it needs Bun,
Node, Python and Go installed.

Before you open a pull request, run what CI runs:

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all
```

If you touched the Go SDK, also run this in `sdk/go`:

```sh
gofmt -l .
go vet ./...
go test ./...
```

[docs/architecture.md](docs/architecture.md) explains how the pieces fit
together. [docs/conformance.md](docs/conformance.md) covers the SDK
conformance suite.

- A change to behaviour should come with a test and, where users see it, an
  update to the matching page in `docs/`.
- A change to the rules a route enforces should show up in `unsworn plan`
  output. See [docs/plan.md](docs/plan.md).

## License

By contributing, you agree that your contribution is licensed under the
[MIT License](LICENSE), the same license as the project.
