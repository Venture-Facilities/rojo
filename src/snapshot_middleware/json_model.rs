use std::{borrow::Cow, path::Path, str};

use anyhow::Context;
use indexmap::IndexMap;
use memofs::Vfs;
use rbx_dom_weak::{
    types::{Attributes, Ref, Variant},
    HashMapExt as _, Ustr, UstrMap,
};
use serde::{Deserialize, Serialize};

use crate::{
    json,
    resolution::{normalize_json_value_for_property, UnresolvedValue},
    snapshot::{InstanceContext, InstanceSnapshot},
    syncback::{filter_properties_preallocated, FsSnapshot, SyncbackReturn, SyncbackSnapshot},
    RojoRef,
};

pub fn snapshot_json_model(
    context: &InstanceContext,
    vfs: &Vfs,
    path: &Path,
    name: &str,
) -> anyhow::Result<Option<InstanceSnapshot>> {
    let contents = vfs.read(path)?;
    let contents_str = str::from_utf8(&contents)
        .with_context(|| format!("File was not valid UTF-8: {}", path.display()))?;

    if contents_str.trim().is_empty() {
        return Ok(None);
    }

    let mut json_value = json::parse_value_with_context(contents_str, || {
        format!("File is not a valid JSON model: {}", path.display())
    })?;

    normalize_json_model_property_values(&mut json_value);

    let mut instance: JsonModel = serde_json::from_value(json_value).with_context(|| {
        format!("File is not a valid JSON model: {}", path.display())
    })?;

    if let Some(top_level_name) = &instance.name {
        let new_name = format!("{}.model.json", top_level_name);

        log::warn!(
            "Model at path {} had a top-level Name field. \
            This field has been ignored since Rojo 6.0.\n\
            Consider removing this field and renaming the file to {}.",
            new_name,
            path.display()
        );
    }

    instance.name = Some(name.to_owned());

    let id = instance.id.take().map(RojoRef::new);
    let schema = instance.schema.take();

    let mut snapshot = instance
        .into_snapshot()
        .with_context(|| format!("Could not load JSON model: {}", path.display()))?;

    snapshot.metadata = snapshot
        .metadata
        .instigating_source(path)
        .relevant_paths(vec![vfs.canonicalize(path)?])
        .context(context)
        .specified_id(id)
        .schema(schema);

    Ok(Some(snapshot))
}

fn normalize_json_model_property_values(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            let class_name = map
                .get("className")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);

            let properties = map.get_mut("properties");

            if let (Some(class_name), Some(serde_json::Value::Object(properties_map))) =
                (class_name, properties)
            {
                for (prop_name, prop_value) in properties_map {
                    normalize_json_value_for_property(&class_name, prop_name, prop_value);
                }
            }

            let children = map.get_mut("children");

            if let Some(children) = children {
                normalize_json_model_property_values(children);
            }
        }
        serde_json::Value::Array(arr) => {
            for child in arr {
                normalize_json_model_property_values(child);
            }
        }
        _ => {}
    }
}

pub fn syncback_json_model<'sync>(
    snapshot: &SyncbackSnapshot<'sync>,
) -> anyhow::Result<SyncbackReturn<'sync>> {
    let mut property_buffer = Vec::with_capacity(snapshot.new_inst().properties.len());

    let mut model = json_model_from_pair(snapshot, &mut property_buffer, snapshot.new);
    // We don't need the name on the root, but we do for children.
    model.name = None;

    if let Some(old_inst) = snapshot.old_inst() {
        // TODO: Is it worth this being an Arc or Rc? I doubt that enough
        // schemas will ever exist in one project for it to matter, but it
        // could have a performance cost.
        model.schema = old_inst.metadata().schema.clone();
    }

    Ok(SyncbackReturn {
        fs_snapshot: FsSnapshot::new().with_added_file(
            &snapshot.path,
            serde_json::to_vec_pretty(&model).context("failed to serialize new JSON Model")?,
        ),
        children: Vec::new(),
        removed_children: Vec::new(),
    })
}

fn json_model_from_pair<'sync>(
    snapshot: &SyncbackSnapshot<'sync>,
    prop_buffer: &mut Vec<(Ustr, &'sync Variant)>,
    new: Ref,
) -> JsonModel {
    let new_inst = snapshot
        .get_new_instance(new)
        .expect("all new referents passed to json_model_from_pair should exist");

    filter_properties_preallocated(snapshot.project(), new_inst, prop_buffer);

    let mut properties = IndexMap::new();
    let mut attributes = IndexMap::new();
    for (name, value) in prop_buffer.drain(..) {
        match value {
            Variant::Attributes(attrs) => {
                for (attr_name, attr_value) in attrs.iter() {
                    // We (probably) don't want to preserve internal attributes,
                    // only user defined ones.
                    if attr_name.starts_with("RBX") {
                        continue;
                    }
                    attributes.insert(
                        attr_name.clone(),
                        UnresolvedValue::from_variant_unambiguous(attr_value.clone()),
                    );
                }
            }
            _ => {
                properties.insert(
                    name,
                    UnresolvedValue::from_variant(value.clone(), &new_inst.class, &name),
                );
            }
        }
    }

    let mut children = Vec::with_capacity(new_inst.children().len());

    for new_child_ref in new_inst.children() {
        children.push(json_model_from_pair(snapshot, prop_buffer, *new_child_ref))
    }

    JsonModel {
        name: Some(new_inst.name.clone()),
        class_name: new_inst.class,
        children,
        properties,
        attributes,
        id: None,
        schema: None,
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonModel {
    #[serde(rename = "$schema", skip_serializing_if = "Option::is_none")]
    schema: Option<String>,

    #[serde(alias = "Name", skip_serializing_if = "Option::is_none")]
    name: Option<String>,

    #[serde(alias = "ClassName")]
    class_name: Ustr,

    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,

    #[serde(
        alias = "Children",
        default = "Vec::new",
        skip_serializing_if = "Vec::is_empty"
    )]
    children: Vec<JsonModel>,

    #[serde(
        alias = "Properties",
        default,
        skip_serializing_if = "IndexMap::is_empty"
    )]
    properties: IndexMap<Ustr, UnresolvedValue>,

    #[serde(default = "IndexMap::new", skip_serializing_if = "IndexMap::is_empty")]
    attributes: IndexMap<String, UnresolvedValue>,
}

impl JsonModel {
    fn into_snapshot(self) -> anyhow::Result<InstanceSnapshot> {
        let name = self.name.unwrap_or_else(|| self.class_name.to_owned());
        let class_name = self.class_name;

        let mut children = Vec::with_capacity(self.children.len());
        for child in self.children {
            children.push(child.into_snapshot()?);
        }

        let mut properties = UstrMap::with_capacity(self.properties.len());
        for (key, unresolved) in self.properties {
            let value = unresolved.resolve(&class_name, &key)?;

            if class_name == "StyleRule"
                && key == Ustr::from("PropertiesSerialize")
                && matches!(&value, Variant::Attributes(attrs) if attrs.is_empty())
            {
                continue;
            }

            properties.insert(key, value);
        }

        if !self.attributes.is_empty() {
            let mut attributes = Attributes::new();

            for (key, unresolved) in self.attributes {
                let value = unresolved.resolve_unambiguous()?;
                attributes.insert(key, value);
            }

            properties.insert("Attributes".into(), attributes.into());
        }

        Ok(InstanceSnapshot {
            snapshot_id: Ref::none(),
            metadata: Default::default(),
            name: Cow::Owned(name),
            class_name,
            properties,
            children,
        })
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use rbx_dom_weak::types::Color3;
    use memofs::{InMemoryFs, VfsSnapshot};

    #[test]
    fn model_from_vfs() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/foo.model.json",
            VfsSnapshot::file(
                r#"
                    {
                      "className": "IntValue",
                      "properties": {
                        "Value": 5
                      },
                      "children": [
                        {
                          "name": "The Child",
                          "className": "StringValue"
                        }
                      ]
                    }
                "#,
            ),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_json_model(
            &InstanceContext::default(),
            &vfs,
            Path::new("/foo.model.json"),
            "foo",
        )
        .unwrap()
        .unwrap();

        insta::assert_yaml_snapshot!(instance_snapshot);
    }

    #[test]
    fn model_from_vfs_legacy() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/foo.model.json",
            VfsSnapshot::file(
                r#"
                    {
                      "ClassName": "IntValue",
                      "Properties": {
                        "Value": 5
                      },
                      "Children": [
                        {
                          "Name": "The Child",
                          "ClassName": "StringValue"
                        }
                      ]
                    }
                "#,
            ),
        )
        .unwrap();

        let vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_json_model(
            &InstanceContext::default(),
            &vfs,
            Path::new("/foo.model.json"),
            "foo",
        )
        .unwrap()
        .unwrap();

        insta::assert_yaml_snapshot!(instance_snapshot);
    }

        #[test]
        fn model_from_vfs_style_rule_properties_serialize_color3uint8() {
                let mut imfs = InMemoryFs::new();
                imfs.load_snapshot(
                        "/foo.model.json",
                        VfsSnapshot::file(
                                r#"
                                        {
                                            "className": "StyleRule",
                                            "properties": {
                                                "PropertiesSerialize": {
                                                    "Attributes": {
                                                        "TextColor3": {
                                                            "Color3uint8": [255, 0, 127]
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                "#,
                        ),
                )
                .unwrap();

                let vfs = Vfs::new(imfs);

                let instance_snapshot = snapshot_json_model(
                        &InstanceContext::default(),
                        &vfs,
                        Path::new("/foo.model.json"),
                        "foo",
                )
                .unwrap()
                .unwrap();

                let props = match instance_snapshot
                        .properties
                        .get(&Ustr::from("PropertiesSerialize"))
                {
                        Some(Variant::Attributes(attrs)) => attrs,
                        other => panic!("Expected PropertiesSerialize to be Variant::Attributes, got {other:?}"),
                };

                assert!(matches!(
                        props.get("TextColor3"),
                        Some(Variant::Color3(color)) if *color == Color3::new(1.0, 0.0, 127.0 / 255.0)
                ));
        }

            #[test]
            fn model_from_vfs_style_rule_properties_serialize_empty_is_omitted() {
                let mut imfs = InMemoryFs::new();
                imfs.load_snapshot(
                    "/foo.model.json",
                    VfsSnapshot::file(
                        r#"
                            {
                                "className": "StyleRule",
                                "properties": {
                                "PropertiesSerialize": {
                                    "Attributes": {}
                                }
                                }
                            }
                        "#,
                    ),
                )
                .unwrap();

                let vfs = Vfs::new(imfs);

                let instance_snapshot = snapshot_json_model(
                    &InstanceContext::default(),
                    &vfs,
                    Path::new("/foo.model.json"),
                    "foo",
                )
                .unwrap()
                .unwrap();

                assert!(instance_snapshot
                    .properties
                    .get(&Ustr::from("PropertiesSerialize"))
                    .is_none());
            }
}
