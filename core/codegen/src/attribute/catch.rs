use devise::ext::SpanDiagnosticExt;
use devise::{syn, MetaItem, Spanned, Result, FromMeta, Diagnostic};

use crate::http_codegen::{self, Optional};
use crate::proc_macro2::{TokenStream, Span};
use crate::syn_ext::ReturnTypeExt;

/// The raw, parsed `#[catch(code)]` attribute.
#[derive(Debug, FromMeta)]
struct CatchAttribute {
    #[meta(naked)]
    status: CatcherCode
}

/// `Some` if there's a code, `None` if it's `default`.
#[derive(Debug)]
struct CatcherCode(Option<http_codegen::Status>);

impl FromMeta for CatcherCode {
    fn from_meta(meta: &MetaItem) -> Result<Self> {
        if usize::from_meta(meta).is_ok() {
            let status = http_codegen::Status::from_meta(meta)?;
            Ok(CatcherCode(Some(status)))
        } else if let MetaItem::Path(path) = meta {
            if path.is_ident("default") {
                Ok(CatcherCode(None))
            } else {
                Err(meta.span().error("expected `default`"))
            }
        } else {
            let msg = format!("expected integer or identifier, found {}", meta.description());
            Err(meta.span().error(msg))
        }
    }
}

/// This structure represents the parsed `catch` attribute and associated items.
struct CatchParams {
    /// The status associated with the code in the `#[catch(code)]` attribute.
    status: Option<http_codegen::Status>,
    /// The function that was decorated with the `catch` attribute.
    function: syn::ItemFn,
}

fn parse_params(
    args: TokenStream,
    input: proc_macro::TokenStream
) -> Result<CatchParams> {
    let function: syn::ItemFn = syn::parse(input)
        .map_err(Diagnostic::from)
        .map_err(|diag| diag.help("`#[catch]` can only be used on functions"))?;

    let attribute = CatchAttribute::from_meta(&syn::parse2(quote!(catch(#args)))?)
        .map_err(|diag| diag.help("`#[catch]` expects a status code int or `default`: \
                        `#[catch(404)]` or `#[catch(default)]`"))?;

    Ok(CatchParams { status: attribute.status.0, function })
}

pub fn _catch(
    args: proc_macro::TokenStream,
    input: proc_macro::TokenStream
) -> Result<TokenStream> {
    // Parse and validate all of the user's input.
    let catch = parse_params(args.into(), input)?;

    // Gather everything we'll need to generate the catcher.
    let user_catcher_fn = &catch.function;
    let user_catcher_fn_name = catch.function.sig.ident.clone();
    let (vis, catcher_status) = (&catch.function.vis, &catch.status);
    let status_code = Optional(catcher_status.as_ref().map(|s| s.0.code));

    // Variables names we'll use and reuse.
    define_spanned_export!(catch.function.span().into() =>
        __req, __status, _Box, Request, Response, StaticCatcherInfo, Catcher,
        ErrorHandlerFuture, Status);

    // Determine the number of parameters that will be passed in.
    if catch.function.sig.inputs.len() > 2 {
        return Err(catch.function.sig.paren_token.span
            .error("invalid number of arguments: must be zero, one, or two")
            .help("catchers optionally take `&Request` or `Status, &Request`"));
    }

    // This ensures that "Responder not implemented" points to the return type.
    let return_type_span = catch.function.sig.output.ty()
        .map(|ty| ty.span().into())
        .unwrap_or(Span::call_site().into());

    // Set the `req` and `status` spans to that of their respective function
    // arguments for a more correct `wrong type` error span. `rev` to be cute.
    let codegen_args = &[__req, __status];
    let inputs = catch.function.sig.inputs.iter().rev()
        .zip(codegen_args.into_iter())
        .map(|(fn_arg, codegen_arg)| match fn_arg {
            syn::FnArg::Receiver(_) => codegen_arg.respanned(fn_arg.span()),
            syn::FnArg::Typed(a) => codegen_arg.respanned(a.ty.span())
        }).rev();

    // We append `.await` to the function call if this is `async`.
    let dot_await = catch.function.sig.asyncness
        .map(|a| quote_spanned!(a.span().into() => .await));

    let catcher_response = quote_spanned!(return_type_span => {
        let ___responder = #user_catcher_fn_name(#(#inputs),*) #dot_await;
        ::rocket::response::Responder::respond_to(___responder, #__req)?
    });

    // Generate the catcher, keeping the user's input around.
    Ok(quote! {
        #user_catcher_fn

        #[doc(hidden)]
        #[allow(non_camel_case_types)]
        /// Rocket code generated proxy structure.
        #vis struct #user_catcher_fn_name {  }

        /// Rocket code generated proxy static conversion implementation.
        impl From<#user_catcher_fn_name> for #StaticCatcherInfo {
            fn from(_: #user_catcher_fn_name) -> #StaticCatcherInfo {
                fn monomorphized_function<'_b>(
                    #__status: #Status,
                    #__req: &'_b #Request<'_>
                ) -> #ErrorHandlerFuture<'_b> {
                    #_Box::pin(async move {
                        let __response = #catcher_response;
                        #Response::build()
                            .status(#__status)
                            .merge(__response)
                            .ok()
                    })
                }

                #StaticCatcherInfo {
                    code: #status_code,
                    handler: monomorphized_function,
                }
            }
        }

        /// Rocket code generated proxy conversion implementation.
        impl From<#user_catcher_fn_name> for #Catcher {
            #[inline]
            fn from(_: #user_catcher_fn_name) -> #Catcher {
                #StaticCatcherInfo::from(#user_catcher_fn_name {}).into()
            }
        }
    })
}

pub fn catch_attribute(
    args: proc_macro::TokenStream,
    input: proc_macro::TokenStream
) -> TokenStream {
    _catch(args, input).unwrap_or_else(|d| d.emit_as_item_tokens())
}
