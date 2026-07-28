//! Workflow registration. Each workflow lives in its own file; register it here
//! so it shows up in the GUI.

use std::sync::Arc;

use crate::registry::WorkflowRegistry;

//pub mod complete;
//pub mod feather;
//pub mod solve;
//add this google_test in test_first!

pub mod complete;
pub mod feather;
pub mod solve;

pub mod first_test;


/// Register every built-in workflow.
pub fn register_all(registry: &mut WorkflowRegistry) {

    /*
    registry.register(Arc::new(feather::NavigateVerify));
    registry.register(Arc::new(feather::NavigateHome));
    registry.register(Arc::new(feather::ClaimTask));
    registry.register(Arc::new(feather::NavigateToTask));
    registry.register(Arc::new(feather::GetTaskData));
    registry.register(Arc::new(feather::SubmitFill));
    registry.register(Arc::new(feather::OpenVmTerminal));
    registry.register(Arc::new(feather::CreateVagonLog));
    registry.register(Arc::new(feather::StopVagon));

    registry.register(Arc::new(solve::BuildImage));
    registry.register(Arc::new(solve::SolvePreflight));
    registry.register(Arc::new(solve::SolveTask));
    registry.register(Arc::new(solve::SolveFormatReview));
    registry.register(Arc::new(solve::SolveCheckpoints));

    registry.register(Arc::new(complete::CompleteTask));
    registry.register(Arc::new(complete::ExecuteOnVm));
    */

    //new!!
  // registry.register(Arc::new(first_test::SayHello));
registry.register(Arc::new(first_test::OpenMultimango));
registry.register(Arc::new(first_test::CreateTask1));
registry.register(Arc::new(first_test::SaveTaskData));
registry.register(Arc::new(first_test::CreateResponseADir));
registry.register(Arc::new(first_test::DownloadResponseA));
registry.register(Arc::new(first_test::DownloadResponseB));
registry.register(Arc::new(first_test::SaveEvaluationCriteria));
registry.register(Arc::new(first_test::AnswerAndApplyCriteria));
registry.register(Arc::new(first_test::HandshakeReviewAndSubmit));

  
}
