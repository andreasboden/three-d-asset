use crate::{animation::*, geometry::*, io::*, material::*, Error, Model, Result};
use ::gltf::Gltf;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub fn dependencies(raw_assets: &RawAssets, path: &PathBuf) -> HashSet<PathBuf> {
    let mut dependencies = HashSet::new();
    if let Ok(Gltf { document, .. }) = Gltf::from_slice(raw_assets.get(path).unwrap()) {
        let base_path = path.parent().unwrap_or(Path::new(""));
        for buffer in document.buffers() {
            match buffer.source() {
                ::gltf::buffer::Source::Uri(uri) => {
                    if uri.starts_with("data:") {
                        dependencies.insert(PathBuf::from(uri));
                    } else {
                        dependencies.insert(base_path.join(uri));
                    }
                }
                _ => {}
            };
        }

        for texture in document.textures() {
            match texture.source().source() {
                ::gltf::image::Source::Uri { uri, .. } => {
                    if uri.starts_with("data:") {
                        use std::str::FromStr;
                        dependencies.insert(PathBuf::from_str(uri).unwrap());
                    } else {
                        dependencies.insert(base_path.join(uri));
                    }
                }
                _ => {}
            };
        }
    }
    dependencies
}

pub fn deserialize_gltf(raw_assets: &mut RawAssets, path: &PathBuf) -> Result<Model> {
    let Gltf { document, mut blob } = Gltf::from_slice(&raw_assets.remove(path)?)?;
    let base_path = path.parent().unwrap_or(Path::new(""));

    let mut buffers = Vec::new();
    for buffer in document.buffers() {
        let mut data = match buffer.source() {
            ::gltf::buffer::Source::Uri(uri) => {
                if uri.starts_with("data:") {
                    raw_assets.remove(uri)?
                } else {
                    raw_assets.remove(base_path.join(uri))?
                }
            }
            ::gltf::buffer::Source::Bin => blob.take().ok_or(Error::GltfMissingData)?,
        };
        if data.len() < buffer.length() {
            Err(Error::GltfCorruptData)?;
        }
        while data.len() % 4 != 0 {
            data.push(0);
        }
        buffers.push(::gltf::buffer::Data(data));
    }

    let mut materials = Vec::new();
    for material in document.materials() {
        materials.push(parse_material(
            raw_assets,
            &base_path,
            &mut buffers,
            &material,
        )?);
    }

    let mut geometries = Vec::new();
    for scene in document.scenes() {
        for node in scene.nodes() {
            parse_tree(&Mat4::identity(), &node, &buffers, &mut geometries)?;
        }
    }

    let mut animations = Vec::new();
    for animation in document.animations() {
        for channel in animation.channels() {
            let reader = channel.reader(|buffer| Some(&buffers[buffer.index()]));
            let input = reader.read_inputs().unwrap().collect::<Vec<_>>();
            dbg!(&input);

            if let ::gltf::animation::util::ReadOutputs::Rotations(rotations) =
                reader.read_outputs().unwrap()
            {
                dbg!(&rotations);
                animations.push(KeyFrames {
                    times: input,
                    rotations: rotations
                        .into_f32()
                        .map(|v| v.into())
                        .collect::<Vec<Quat>>(),
                });
            }
        }
    }
    dbg!(&animations);
    Ok(Model {
        geometries,
        materials,
        animations,
    })
}

fn parse_tree<'a>(
    parent_transform: &Mat4,
    node: &::gltf::Node,
    buffers: &[::gltf::buffer::Data],
    geometries: &mut Vec<TriMesh>,
) -> Result<()> {
    let node_transform = parse_transform(node.transform());
    if node_transform.determinant() == 0.0 {
        return Ok(()); // glTF say that if the scale is all zeroes, the node should be ignored.
    }
    let transform = parent_transform * node_transform;

    if let Some(mesh) = node.mesh() {
        let name: String = mesh
            .name()
            .map(|s| s.to_string())
            .unwrap_or(format!("index {}", mesh.index()));
        for primitive in mesh.primitives() {
            geometries.push(parse_mesh(name.clone(), transform, buffers, &primitive)?);
        }
    }

    for child in node.children() {
        parse_tree(&transform, &child, buffers, geometries)?;
    }
    Ok(())
}

fn parse_mesh(
    name: String,
    transform: Mat4,
    buffers: &[::gltf::buffer::Data],
    primitive: &::gltf::mesh::Primitive,
) -> Result<TriMesh> {
    let reader = primitive.reader(|buffer| Some(&buffers[buffer.index()]));
    if let Some(read_positions) = reader.read_positions() {
        let positions: Vec<_> = read_positions.map(|p| p.into()).collect();

        let normals = reader
            .read_normals()
            .map(|values| values.map(|n| n.into()).collect());

        let tangents = reader
            .read_tangents()
            .map(|values| values.map(|t| t.into()).collect());

        let indices = reader
            .read_indices()
            .map(|values| match values {
                ::gltf::mesh::util::ReadIndices::U8(iter) => Indices::U8(iter.collect()),
                ::gltf::mesh::util::ReadIndices::U16(iter) => Indices::U16(iter.collect()),
                ::gltf::mesh::util::ReadIndices::U32(iter) => Indices::U32(iter.collect()),
            })
            .unwrap_or(Indices::None);

        let colors = reader.read_colors(0).map(|values| {
            values
                .into_rgba_u8()
                .map(|c| Color::new(c[0], c[1], c[2], c[3]))
                .collect()
        });

        let uvs = reader
            .read_tex_coords(0)
            .map(|values| values.into_f32().map(|uv| uv.into()).collect());

        let mut mesh = TriMesh {
            name: name.clone(),
            positions: Positions::F32(positions),
            normals,
            tangents,
            indices,
            colors,
            uvs,
            material_name: Some(material_name(&primitive.material())),
        };
        if transform != Mat4::identity() {
            mesh.transform(&transform)?;
        }
        Ok(mesh)
    } else {
        unreachable!()
    }
}

fn material_name(material: &::gltf::material::Material) -> String {
    material.name().map(|s| s.to_string()).unwrap_or(
        material
            .index()
            .map(|i| format!("index {}", i))
            .unwrap_or("default".to_string()),
    )
}

fn parse_material(
    raw_assets: &mut RawAssets,
    path: &Path,
    buffers: &[::gltf::buffer::Data],
    material: &::gltf::material::Material,
) -> Result<PbrMaterial> {
    let pbr = material.pbr_metallic_roughness();
    let color = pbr.base_color_factor();
    let albedo_texture = if let Some(info) = pbr.base_color_texture() {
        Some(parse_texture(raw_assets, path, buffers, info.texture())?)
    } else {
        None
    };
    let metallic_roughness_texture = if let Some(info) = pbr.metallic_roughness_texture() {
        Some(parse_texture(raw_assets, path, buffers, info.texture())?)
    } else {
        None
    };
    let (normal_texture, normal_scale) = if let Some(normal) = material.normal_texture() {
        (
            Some(parse_texture(raw_assets, path, buffers, normal.texture())?),
            normal.scale(),
        )
    } else {
        (None, 1.0)
    };
    let (occlusion_texture, occlusion_strength) =
        if let Some(occlusion) = material.occlusion_texture() {
            (
                Some(parse_texture(
                    raw_assets,
                    path,
                    buffers,
                    occlusion.texture(),
                )?),
                occlusion.strength(),
            )
        } else {
            (None, 1.0)
        };
    let emissive_texture = if let Some(info) = material.emissive_texture() {
        Some(parse_texture(raw_assets, path, buffers, info.texture())?)
    } else {
        None
    };
    let transmission_texture =
        if let Some(Some(info)) = material.transmission().map(|t| t.transmission_texture()) {
            Some(parse_texture(raw_assets, path, buffers, info.texture())?)
        } else {
            None
        };
    Ok(PbrMaterial {
        name: material_name(material),
        albedo: Color::from_rgba_slice(&color),
        albedo_texture,
        metallic: pbr.metallic_factor(),
        roughness: pbr.roughness_factor(),
        metallic_roughness_texture,
        normal_texture,
        normal_scale,
        occlusion_texture,
        occlusion_strength,
        occlusion_metallic_roughness_texture: None,
        emissive: Color::from_rgb_slice(&material.emissive_factor()),
        emissive_texture,
        transmission: material
            .transmission()
            .map(|t| t.transmission_factor())
            .unwrap_or(0.0),
        transmission_texture,
        index_of_refraction: material.ior().unwrap_or(1.5),
        alpha_cutout: material.alpha_cutoff(),
        lighting_model: LightingModel::Cook(
            NormalDistributionFunction::TrowbridgeReitzGGX,
            GeometryFunction::SmithSchlickGGX,
        ),
    })
}

fn parse_texture<'a>(
    raw_assets: &mut RawAssets,
    path: &Path,
    buffers: &[::gltf::buffer::Data],
    gltf_texture: ::gltf::texture::Texture,
) -> Result<Texture2D> {
    let gltf_image = gltf_texture.source();
    let gltf_source = gltf_image.source();
    let tex = match gltf_source {
        ::gltf::image::Source::Uri { uri, .. } => {
            if uri.starts_with("data:") {
                raw_assets.deserialize(uri)?
            } else {
                raw_assets.deserialize(path.join(uri))?
            }
        }
        ::gltf::image::Source::View { view, .. } => {
            if view.stride() != None {
                unimplemented!();
            }
            #[allow(unused_variables)]
            let buffer = &buffers[view.buffer().index()];
            #[cfg(not(feature = "image"))]
            return Err(Error::FeatureMissing("image".to_string()));
            #[cfg(feature = "image")]
            super::img::deserialize_img("", &buffer[view.offset()..view.offset() + view.length()])?
        }
    };
    // TODO: Parse sampling parameters
    Ok(tex)
}

fn parse_transform(transform: ::gltf::scene::Transform) -> Mat4 {
    let [c0, c1, c2, c3] = transform.matrix();
    Mat4::from_cols(c0.into(), c1.into(), c2.into(), c3.into())
}

#[cfg(test)]
mod test {

    #[test]
    pub fn load_gltf() {
        let mut loaded = crate::io::load(&["test_data/Cube.gltf"]).unwrap();
        let model: crate::Model = loaded.deserialize(".gltf").unwrap();
        assert_eq!(
            model.materials[0]
                .albedo_texture
                .as_ref()
                .map(|t| std::path::PathBuf::from(&t.name)),
            Some(std::path::PathBuf::from("test_data/Cube_BaseColor.png"))
        );
        assert_eq!(
            model.materials[0]
                .metallic_roughness_texture
                .as_ref()
                .map(|t| std::path::PathBuf::from(&t.name)),
            Some(std::path::PathBuf::from(
                "test_data/Cube_MetallicRoughness.png"
            ))
        );
    }

    #[test]
    pub fn deserialize_gltf() {
        let model: crate::Model = crate::io::RawAssets::new()
            .insert(
                "Cube.gltf",
                include_bytes!("../../test_data/Cube.gltf").to_vec(),
            )
            .insert(
                "Cube.bin",
                include_bytes!("../../test_data/Cube.bin").to_vec(),
            )
            .insert(
                "Cube_BaseColor.png",
                include_bytes!("../../test_data/Cube_BaseColor.png").to_vec(),
            )
            .insert(
                "Cube_MetallicRoughness.png",
                include_bytes!("../../test_data/Cube_MetallicRoughness.png").to_vec(),
            )
            .deserialize("gltf")
            .unwrap();
        assert_eq!(model.geometries.len(), 1);
        assert_eq!(model.materials.len(), 1);
        assert_eq!(
            model.materials[0]
                .albedo_texture
                .as_ref()
                .map(|t| t.name.as_str()),
            Some("Cube_BaseColor.png")
        );
        assert_eq!(
            model.materials[0]
                .metallic_roughness_texture
                .as_ref()
                .map(|t| t.name.as_str()),
            Some("Cube_MetallicRoughness.png")
        );
    }

    #[test]
    pub fn deserialize_gltf_with_data_url() {
        let model: crate::Model =
            crate::io::load_and_deserialize("test_data/data_url.gltf").unwrap();
        assert_eq!(model.geometries.len(), 1);
        assert_eq!(model.materials.len(), 1);
    }

    #[test]
    pub fn deserialize_gltf_with_animations() {
        let model: crate::Model =
            crate::io::load_and_deserialize("test_data/AnimatedTriangle.gltf").unwrap();
        assert_eq!(model.geometries.len(), 2);
        assert_eq!(model.materials.len(), 1);
    }
}
