use std::collections::HashMap;
use tonic::codegen::Body;
use crate::cells::{TemplateCell, TextRange};
use crate::execution::primitives::operation::{InputItemConfiguration, InputSignature, InputType, OperationFnOutput, OperationNode, OutputItemConfiguration, OutputSignature};
use crate::execution::primitives::serialized_value::{RkyvSerializedValue as RKV, serialized_value_to_json_value};

use futures_util::FutureExt;

/// Template cells leverage the same tooling as LLM Prompt Cells, but are used for more general templating.
#[tracing::instrument]
pub fn template_cell(cell: &TemplateCell, range: &TextRange) -> anyhow::Result<OperationNode> {
    let schema =
        chidori_prompt_format::templating::templates::analyze_referenced_partials(&cell.body);

    let mut input_signature = InputSignature::new();
    for (key, value) in &schema.items {
        input_signature.globals.insert(
            key.clone(),
            InputItemConfiguration {
                ty: Some(InputType::String),
                default: None,
            },
        );
    }


    let mut output_signature = OutputSignature::new();
    if let Some(name) = &cell.name {
        output_signature.functions.insert(
            name.clone(),
            OutputItemConfiguration::Function {
                input_signature: InputSignature::new(),
                emit_event: vec![],
                trigger_on: vec![],
            },
        );
    }

    let body = cell.body.clone();
    Ok(OperationNode::new(
        cell.name.clone(),
        input_signature,
        output_signature,
        Box::new(move |_, x, _, _| {
            let body = body.clone();
            async move {
                let data = if let RKV::Object(m) = x {
                    if let Some(m) = m.get("globals") {
                        serialized_value_to_json_value( m )
                    } else {
                        serialized_value_to_json_value(&RKV::Null)
                    }
                } else {
                    serialized_value_to_json_value(&x)
                };
                let rendered = chidori_prompt_format::templating::templates::render_template_prompt(&body, &data, &HashMap::new()).unwrap();
                Ok(OperationFnOutput::with_value(RKV::String(rendered)))
            }.boxed()
        }),
    ))
}


#[cfg(test)]
mod test {
    use crate::cells::TextRange;
    use crate::execution::execution::ExecutionState;

    #[tokio::test]
    async fn test_template_cell() -> anyhow::Result<()> {
        let cell = crate::cells::TemplateCell {
            name: Some("test".to_string()),
            body: "Hello, {{ name }}!".to_string(),
        };
        let op = crate::cells::template_cell::template_cell(&cell, &TextRange::default())?;
        let input = crate::execution::primitives::serialized_value::RkyvSerializedValue::Object(
            std::collections::HashMap::new()
        );
        let output = op.execute(&ExecutionState::new(), input, None, None).await?;
        assert_eq!(output.output, crate::execution::primitives::serialized_value::RkyvSerializedValue::String("Hello, !".to_string()));
        Ok(())
    }
}