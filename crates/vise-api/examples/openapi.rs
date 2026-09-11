use std::fs;

use serde_json::Value;
use utoipa::OpenApi;
use vise_api::openapi::ApiDoc;

fn normalize_openapi(value: &mut Value) {
    match value {
        Value::Object(map) => {
            // OpenAPI 3.1 represents nullable strings as:
            //
            //   "type": ["string", "null"]
            //
            // Convert that to the OpenAPI 3.0 representation:
            //
            //   "type": "string",
            //   "nullable": true
            if let Some(Value::Array(types)) = map.get("type") {
                let has_null = types.iter().any(|v| v == "null");

                if has_null {
                    let non_null_types: Vec<&Value> =
                        types.iter().filter(|v| *v != "null").collect();

                    if non_null_types.len() == 1 {
                        map.insert("type".to_string(), non_null_types[0].clone());
                        map.insert("nullable".to_string(), Value::Bool(true));
                    }
                }
            }

            // OpenAPI 3.1 represents nullable refs as:
            //
            //   "oneOf": [{ "type": "null" }, { "$ref": "..." }]
            //
            // Convert to the OpenAPI 3.0 representation:
            //
            //   "allOf": [{ "$ref": "..." }],
            //   "nullable": true
            if let Some(Value::Array(variants)) = map.get("oneOf") {
                let non_null_variants: Vec<Value> = variants
                    .iter()
                    .filter(|v| v.get("type") != Some(&Value::String("null".to_string())))
                    .cloned()
                    .collect();

                if variants.len() == 2 && non_null_variants.len() == 1 {
                    let variant = &non_null_variants[0];

                    map.remove("oneOf");

                    if let Some(reference) = variant.get("$ref") {
                        map.insert(
                            "allOf".to_string(),
                            Value::Array(vec![
                                serde_json::json!({ "$ref": reference.clone() }),
                            ]),
                        );
                        if let Some(description) = variant.get("description") {
                            map.insert("description".to_string(), description.clone());
                        }
                    } else if let Value::Object(fields) = variant {
                        for (key, value) in fields {
                            map.insert(key.clone(), value.clone());
                        }
                    }

                    map.insert("nullable".to_string(), Value::Bool(true));
                }
            }

            for value in map.values_mut() {
                normalize_openapi(value);
            }
        }

        Value::Array(items) => {
            for item in items {
                normalize_openapi(item);
            }
        }

        _ => {}
    }
}

fn main() -> anyhow::Result<()> {
    let spec = ApiDoc::openapi();

    let mut json = serde_json::to_value(spec)?;

    normalize_openapi(&mut json);

    // Progenitor currently expects OpenAPI 3.0-style documents.
    json["openapi"] = Value::String("3.0.3".to_string());

    fs::create_dir_all("openapi")?;

    fs::write("openapi/openapi.json", serde_json::to_string_pretty(&json)?)?;

    println!("Generated openapi/openapi.json");

    Ok(())
}
