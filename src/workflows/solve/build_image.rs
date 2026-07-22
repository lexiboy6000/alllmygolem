//! "Solve: build ngspice image" — (re)build the golem-ngspice Docker image from
//! the embedded Dockerfile on demand. (Preflight also builds it automatically if
//! it's missing; this lets you rebuild explicitly, e.g. after editing it.)

use std::time::Duration;

use crate::prelude::*;

use super::util;

pub struct BuildImage;

#[async_trait]
impl Workflow for BuildImage {
    fn name(&self) -> &'static str {
        "Solve: build ngspice image"
    }
    fn description(&self) -> &'static str {
        "(Re)build the golem-ngspice Docker image (Ubuntu + ngspice + gnuplot) from the Dockerfile."
    }
    fn requires_browser(&self) -> bool {
        false
    }

    async fn run(&self, ctx: &mut WorkflowCtx) -> Result<WorkflowOutcome> {
        ctx.step("check Docker").await?;
        let info_ok = matches!(
            ctx.run("docker", &["info"], None, Some(Duration::from_secs(25))).await,
            Ok(o) if o.success()
        );
        if !info_ok {
            return Err(ctx
                .stop_and_warn("Docker daemon not reachable. Start Docker and retry.")
                .await);
        }

        ctx.step("build ngspice image").await?;
        let image = util::image_tag(&ctx.settings);
        ctx.output(format!("building {image} (downloads packages on first build)..."));
        if let Err(e) = util::build_image(ctx).await {
            return Err(ctx.stop_and_warn(e.to_string()).await);
        }

        ctx.step("verify image").await?;
        let ok = matches!(
            ctx.run(
                "docker",
                &[
                    "run", "--rm", image.as_str(), "sh", "-c",
                    "ngspice -v >/dev/null 2>&1 && gnuplot --version",
                ],
                None,
                Some(Duration::from_secs(90)),
            )
            .await,
            Ok(o) if o.success()
        );
        if !ok {
            return Err(ctx
                .stop_and_warn(format!("image {image} built but the ngspice/gnuplot check failed"))
                .await);
        }
        ctx.output(format!("image {image} ready"));
        Ok(WorkflowOutcome::Completed)
    }
}
