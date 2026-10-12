//! `#[gpui_hot::hot]`; see the gpui-hot crate.

use proc_macro::TokenStream;
use quote::quote;
use syn::{FnArg, ImplItem, ItemImpl, Pat, Type, parse_macro_input, parse_quote};

/// On an `impl Render for View` block: runs the body of `render` through
/// `gpui_hot::call`, so patches reach views created before them.
#[proc_macro_attribute]
pub fn hot(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return syn::Error::new(proc_macro2::Span::call_site(), "#[hot] takes no arguments")
            .to_compile_error()
            .into();
    }
    let mut block = parse_macro_input!(item as ItemImpl);
    let mut found = false;
    for item in &mut block.items {
        let ImplItem::Fn(f) = item else { continue };
        if f.sig.ident != "render" {
            continue;
        }
        found = true;
        let body = &f.block;
        // Fresh reborrows of `window` and `cx` in each call, so the body may
        // move them as it could in a plain `render`.
        let reborrows = f.sig.inputs.iter().filter_map(|arg| {
            let FnArg::Typed(arg) = arg else { return None };
            let (Pat::Ident(name), Type::Reference(ty)) = (&*arg.pat, &*arg.ty) else {
                return None;
            };
            let name = &name.ident;
            ty.mutability.is_some().then(|| quote!(let #name = &mut *#name;))
        });
        // The inner closure keeps `return` in the body meaning "return from
        // render"; the outer one is the hot function, monomorphised here, in
        // the crate that patches rebuild.
        f.block = parse_quote!({
            ::gpui_hot::call(|| {
                #(#reborrows)*
                ::gpui_hot::gpui::IntoElement::into_any_element((|| #body)())
            })
        });
    }
    if !found {
        return syn::Error::new_spanned(&block.self_ty, "#[hot] expects an impl with `fn render`")
            .to_compile_error()
            .into();
    }
    quote!(#block).into()
}
