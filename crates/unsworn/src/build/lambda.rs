//! `--target lambda`: the code of an AWS Lambda function, for whatever
//! deploys it (SAM, the CDK, Terraform, a zip upload). Written, by default,
//! to `dist/lambda`: `index.mjs` and what it loads (see
//! [`super::functions`]), with nothing else in the directory, so it can be
//! zipped or used as a code directory as it is.
//!
//! The bundle exports two handlers: `index.handler` answers whole, for API
//! Gateway and function URLs, and `index.streamHandler` streams, for
//! function URLs with `RESPONSE_STREAM`. The function, its URL or API, and
//! its settings belong to the user's own infrastructure code.

use super::{Prepared, functions};

pub fn build(p: Prepared) -> Result<(), String> {
    let entry = functions::entry(
        &p,
        &p.out,
        "import { createLambdaHandlers } from \"@unswornjs/sdk/lambda\";",
        "export const { handler, streamHandler } = createLambdaHandlers",
    )?;
    let built = functions::build(&p, &p.out, &entry)?;
    p.report(
        built.assets,
        &[
            ("runtime:", "nodejs22.x or newer, as an ES module"),
            (
                "handler:",
                "index.handler, or index.streamHandler for a function URL with RESPONSE_STREAM",
            ),
            (
                "routes:",
                "send every path to the function ($default on an HTTP API, or a function URL)",
            ),
        ],
    );
    Ok(())
}
