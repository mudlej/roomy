//! Reading the arguments of a call from QEMU.

/// Argument `index` of a call, as a `T`.
pub(crate) fn arg<T: glib::variant::FromVariant>(parameters: &glib::Variant, index: usize) -> Result<T, String> {
    if index >= parameters.n_children() {
        return Err(format!("argument {index} is missing"));
    }
    parameters.child_value(index).get().ok_or_else(|| format!("argument {index} has an unexpected type"))
}

/// Argument `index` of a call, a byte array.
pub(crate) fn bytes(parameters: &glib::Variant, index: usize) -> Result<Vec<u8>, String> {
    if index >= parameters.n_children() {
        return Err(format!("argument {index} is missing"));
    }
    let data = parameters.child_value(index);
    data.fixed_array::<u8>().map(<[u8]>::to_vec).map_err(|_| format!("argument {index} is not a byte array"))
}
