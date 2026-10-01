use super::{
    layout as gang_up_layout,
    model::{BleedOption, LayoutMode, PresetInput, SizeInches},
};
use crate::error::{AppError, AppResult};

const MAX_NAME_BYTES: usize = 200;

pub(crate) fn validate_preset_input(input: &PresetInput) -> AppResult<()> {
    if input
        .source_bleed_override
        .is_some_and(|b| !b.is_finite() || !(0.001..=1.0).contains(&b))
    {
        return Err(AppError::bad_request(
            "preset source bleed override must be from 0.001 to 1 inch",
        ));
    }
    if input.artwork_fit.is_some_and(|fit| {
        !fit.position.x.is_finite()
            || !fit.position.y.is_finite()
            || !(0.0..=1.0).contains(&fit.position.x)
            || !(0.0..=1.0).contains(&fit.position.y)
    }) {
        return Err(AppError::bad_request(
            "preset crop positions must be from 0 to 1",
        ));
    }
    validate_name(&input.name, "preset name")?;
    validate_size(input.finished_cut_size, "preset finished cut size")?;
    validate_size(input.parent_sheet_size, "preset parent sheet size")?;
    if matches!(input.bleed_handling, BleedOption::ScaleToBleed)
        && (!input.created_bleed_amount.is_finite()
            || !(0.001..=1.0).contains(&input.created_bleed_amount))
    {
        return Err(AppError::bad_request(
            "preset created bleed amount must be from 0.001 to 1 inch",
        ));
    }
    if !input.gutter.horizontal.is_finite()
        || !input.gutter.vertical.is_finite()
        || input.gutter.horizontal < 0.0
        || input.gutter.vertical < 0.0
        || input.gutter.horizontal > 100.0
        || input.gutter.vertical > 100.0
    {
        return Err(AppError::bad_request(
            "preset gutters must be finite values from 0 to 100 inches",
        ));
    }
    match (input.layout_preference, input.manual) {
        (LayoutMode::Manual, Some(manual)) => gang_up_layout::validate_manual_layout(manual)?,
        (LayoutMode::Manual, None) => {
            return Err(AppError::bad_request(
                "manual presets must include columns, rows, and rotation",
            ));
        }
        (LayoutMode::Auto | LayoutMode::MaxPieces, None) => {}
        (LayoutMode::Auto | LayoutMode::MaxPieces, Some(_)) => {
            return Err(AppError::bad_request(
                "preset custom grid settings require manual layout mode",
            ));
        }
    }
    match (input.sides, input.duplex.as_ref()) {
        (super::model::Sides::Single, None) | (super::model::Sides::Double, Some(_)) => {}
        (super::model::Sides::Single, Some(_)) => {
            return Err(AppError::bad_request(
                "single-sided presets cannot include duplex settings",
            ));
        }
        (super::model::Sides::Double, None) => {
            return Err(AppError::bad_request(
                "double-sided presets require duplex settings",
            ));
        }
    }
    validate_optional_name(input.id.as_deref(), "preset id")
}

fn validate_size(size: SizeInches, label: &str) -> AppResult<()> {
    if !size.width.is_finite()
        || !size.height.is_finite()
        || !(0.01..=100.0).contains(&size.width)
        || !(0.01..=100.0).contains(&size.height)
    {
        return Err(AppError::bad_request(format!(
            "{label} must be from 0.01 to 100 inches in each direction"
        )));
    }
    Ok(())
}

fn validate_optional_name(value: Option<&str>, label: &str) -> AppResult<()> {
    if let Some(value) = value {
        validate_name(value, label)?;
    }
    Ok(())
}

fn validate_name(value: &str, label: &str) -> AppResult<()> {
    let value = value.trim();
    if value.is_empty() {
        return Err(AppError::bad_request(format!("{label} is required")));
    }
    if value.len() > MAX_NAME_BYTES || value.chars().any(char::is_control) {
        return Err(AppError::bad_request(format!(
            "{label} must be at most {MAX_NAME_BYTES} bytes without control characters"
        )));
    }
    Ok(())
}
