use crate::prelude::*;

pub struct SayHello;

#[async_trait]
impl Workflow for SayHello {
    fn name(&self) -> &'static str {
        "Say hello"
    }

    //fn inputs(&self) -> Vec<InputSpec> {
    //    vec![InputSpec::required("name", "Your name")]
    //}

    async fn run(&self, ctx: &mut WorkflowCtx) -> Result<WorkflowOutcome> {
        let name = ctx.require_input("name")?;      // reads the "name" field the user typed
        ctx.output(format!("Hello, {name}!"));       // prints it to the output log
        Ok(WorkflowOutcome::Completed)
    }
}


/*
rust practise...
fn main(){
    println!("hello world!");
}
let name: &str = "lex";
const MAX: u32 = 100;

let x = 3.9;
let x: f32 =3.2;
*/
