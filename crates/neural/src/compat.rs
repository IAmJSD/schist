//! Bringing older ONNX graphs up to something tract can parse.
//!
//! The style-transfer networks in the ONNX model zoo were exported at opset 9
//! and use `Upsample`, an operator deprecated in favour of `Resize` and which
//! tract therefore never implemented. The obvious fix is to convert the files
//! offline and host the results, but that means asking people to trust our
//! bytes rather than the zoo's. So instead we download the official file
//! unmodified, check it against the hash upstream publishes, and do the small
//! amount of surgery here.
//!
//! It really is small. Only two operators in these graphs changed between
//! opset 9 and 10:
//!
//! * `Upsample(X, scales)` became `Resize(X, scales)`. At opset 10 `Resize`
//!   reads scales from input 1 and hard-codes asymmetric coordinate
//!   transformation, which is exactly what `Upsample` did -- so the rewrite is
//!   a rename plus an explicit `nearest_mode` to pin the rounding.
//! * `Slice` moved `starts`/`ends`/`axes` out of attributes and into inputs,
//!   so those attributes become initializers.
//!
//! `Pad`, the only other candidate, did not change until opset 11 and is left
//! alone. Anything already at opset 10 or above is passed through untouched.

use tract_onnx::pb::{
    attribute_proto::AttributeType, tensor_proto::DataType, AttributeProto, ModelProto, NodeProto,
    TensorProto,
};

/// The opset we lift opset-9 graphs to.
const TARGET: i64 = 10;

/// Rewrite `proto` in place if it is too old for tract, returning whether
/// anything was changed. Graphs tract can already read are left exactly as
/// they are, so this is safe to call on every model.
pub fn modernise(proto: &mut ModelProto) -> bool {
    let Some(opset) = proto.opset_import.iter_mut().find(|o| o.domain.is_empty()) else {
        return false;
    };
    if opset.version >= TARGET {
        return false;
    }
    let Some(graph) = proto.graph.as_mut() else {
        return false;
    };
    // Only `Upsample` actually forces our hand. If a graph is old but does not
    // use it, tract's opset-9 parsing is fine and bumping the version would
    // only change the meaning of ops like `Slice` for no reason.
    if !graph.node.iter().any(|n| n.op_type == "Upsample") {
        return false;
    }

    let mut new_initializers = Vec::new();
    for node in graph.node.iter_mut() {
        match node.op_type.as_str() {
            "Upsample" => upsample_to_resize(node),
            "Slice" => slice_attrs_to_inputs(node, &mut new_initializers),
            _ => {}
        }
    }
    graph.initializer.extend(new_initializers);
    opset.version = TARGET;
    true
}

/// `Upsample(X, scales)` -> `Resize(X, scales)`.
fn upsample_to_resize(node: &mut NodeProto) {
    node.op_type = "Resize".to_string();
    // `Upsample` picked the nearest source pixel by truncating, whereas
    // `Resize` defaults to round-half-down. They agree on the integer scale
    // factors these models use, but say what we mean.
    if !node.attribute.iter().any(|a| a.name == "nearest_mode") {
        node.attribute.push(string_attr("nearest_mode", "floor"));
    }
}

/// `Slice` with `starts`/`ends`/`axes` attributes -> the opset-10 form that
/// takes them as inputs. The new inputs are emitted as initializers named
/// after the node's output, which is unique by construction.
fn slice_attrs_to_inputs(node: &mut NodeProto, initializers: &mut Vec<TensorProto>) {
    // Already in the new form.
    if node.input.len() > 1 {
        return;
    }
    let stem = node
        .output
        .first()
        .cloned()
        .unwrap_or_else(|| node.name.clone());
    for name in ["starts", "ends", "axes"] {
        let Some(attr) = node.attribute.iter().find(|a| a.name == name) else {
            // `axes` is optional; missing `starts`/`ends` would be a malformed
            // graph, and leaving the input off lets tract say so.
            continue;
        };
        let input = format!("{stem}_{name}");
        initializers.push(int64_tensor(&input, &attr.ints));
        node.input.push(input);
    }
    node.attribute
        .retain(|a| !matches!(a.name.as_str(), "starts" | "ends" | "axes"));
}

fn string_attr(name: &str, value: &str) -> AttributeProto {
    AttributeProto {
        name: name.to_string(),
        r#type: AttributeType::String as i32,
        s: value.as_bytes().to_vec(),
        ..Default::default()
    }
}

fn int64_tensor(name: &str, values: &[i64]) -> TensorProto {
    TensorProto {
        name: name.to_string(),
        dims: vec![values.len() as i64],
        data_type: DataType::Int64 as i32,
        int64_data: values.to_vec(),
        ..Default::default()
    }
}
