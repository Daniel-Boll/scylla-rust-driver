use darling::{FromAttributes, FromField};
use proc_macro2::Span;
use syn::ext::IdentExt;
use syn::parse_quote;

use super::{DeserializeCommonFieldAttrs, DeserializeCommonStructAttrs};

#[derive(FromAttributes)]
#[darling(attributes(scylla))]
struct StructAttrs {
    #[darling(rename = "crate")]
    crate_path: Option<syn::Path>,
}

impl DeserializeCommonStructAttrs for StructAttrs {
    fn crate_path(&self) -> Option<&syn::Path> {
        self.crate_path.as_ref()
    }
}

#[derive(FromField)]
#[darling(attributes(scylla))]
struct Field {
    // If true, then the field is not parsed at all, but it is initialized
    // with Default::default() instead. All other attributes are ignored.
    #[darling(default)]
    skip: bool,

    // If set, then deserialization will look for the column with given name
    // and deserialize it to this Rust field, instead of just using the Rust
    // field name.
    #[darling(default)]
    rename: Option<String>,

    ident: Option<syn::Ident>,
    ty: syn::Type,
}

impl DeserializeCommonFieldAttrs for Field {
    fn needs_default(&self) -> bool {
        self.skip
    }

    fn deserialize_target(&self) -> &syn::Type {
        &self.ty
    }
}

// derive(DeserializeRow) for the new DeserializeRow trait
pub(crate) fn deserialize_row_derive(
    tokens_input: proc_macro::TokenStream,
) -> Result<syn::ItemImpl, syn::Error> {
    let input = syn::parse(tokens_input)?;

    let implemented_trait: syn::Path = parse_quote! { DeserializeRow };
    let implemented_trait_name = implemented_trait
        .segments
        .last()
        .unwrap()
        .ident
        .unraw()
        .to_string();
    let constraining_trait = parse_quote! { DeserializeValue };
    let s = StructDesc::new(&input, &implemented_trait_name, constraining_trait)?;

    let items = [
        s.generate_type_check_method().into(),
        s.generate_deserialize_method().into(),
    ];

    Ok(s.generate_impl(implemented_trait, items))
}

impl Field {
    // Returns whether this field is mandatory for deserialization.
    fn is_required(&self) -> bool {
        !self.skip
    }

    // A Rust literal representing the name of this field
    fn cql_name_literal(&self) -> syn::LitStr {
        let field_name = match self.rename.as_ref() {
            Some(rename) => rename.to_owned(),
            None => self.ident.as_ref().unwrap().unraw().to_string(),
        };
        syn::LitStr::new(&field_name, Span::call_site())
    }
}

type StructDesc = super::StructDescForDeserialize<StructAttrs, Field>;

impl StructDesc {
    fn generate_type_check_method(&self) -> syn::ImplItemFn {
        TypeCheckUnorderedGenerator(self).generate()
    }

    fn generate_deserialize_method(&self) -> syn::ImplItemFn {
        DeserializeUnorderedGenerator(self).generate()
    }
}

struct TypeCheckUnorderedGenerator<'sd>(&'sd StructDesc);

impl<'sd> TypeCheckUnorderedGenerator<'sd> {
    // An identifier for a bool variable that represents whether given
    // field was already visited during type check
    fn visited_flag_variable(field: &Field) -> syn::Ident {
        quote::format_ident!("visited_{}", field.ident.as_ref().unwrap().unraw())
    }

    // Generates a declaration of a "visited" flag for the purpose of type check.
    // We generate it even if the flag is not required in order to protect
    // from fields appearing more than once
    fn generate_visited_flag_decl(field: &Field) -> Option<syn::Stmt> {
        (!field.skip).then(|| {
            let visited_flag = Self::visited_flag_variable(field);
            parse_quote! {
                let mut #visited_flag = false;
            }
        })
    }

    // Generates code that, given variable `typ`, type-checks given field
    fn generate_type_check(&self, field: &Field) -> Option<syn::Block> {
        (!field.skip).then(|| {
            let macro_internal = self.0.struct_attrs().macro_internal_path();
            let constraint_lifetime = self.0.constraint_lifetime();
            let visited_flag = Self::visited_flag_variable(field);
            let typ = field.deserialize_target();
            let cql_name_literal = field.cql_name_literal();
            let decrement_if_required: Option::<syn::Stmt> = field.is_required().then(|| parse_quote! {
                remaining_required_fields -= 1;
            });

            parse_quote! {
                {
                    if !#visited_flag {
                        <#typ as #macro_internal::DeserializeValue<#constraint_lifetime>>::type_check(&spec.typ)
                            .map_err(|err| {
                                #macro_internal::mk_row_typck_err::<Self>(
                                    column_types_iter(),
                                    #macro_internal::DeserBuiltinRowTypeCheckErrorKind::ColumnTypeCheckFailed {
                                        column_index,
                                        column_name: <_ as ::std::borrow::ToOwned>::to_owned(#cql_name_literal),
                                        err,
                                    }
                                )
                            })?;
                        #visited_flag = true;
                        #decrement_if_required
                    } else {
                        return ::std::result::Result::Err(
                            #macro_internal::mk_row_typck_err::<Self>(
                                column_types_iter(),
                                #macro_internal::DeserBuiltinRowTypeCheckErrorKind::DuplicatedColumn {
                                    column_index,
                                    column_name: #cql_name_literal,
                                }
                            )
                        )
                    }
                }
            }
        })
    }

    // Generates code that appends the flag name if it is missing.
    // The generated code is used to construct a nice error message.
    fn generate_append_name(field: &Field) -> Option<syn::Block> {
        field.is_required().then(|| {
            let visited_flag = Self::visited_flag_variable(field);
            let cql_name_literal = field.cql_name_literal();
            parse_quote! {
                {
                    if !#visited_flag {
                        missing_fields.push(#cql_name_literal);
                    }
                }
            }
        })
    }

    fn generate(&self) -> syn::ImplItemFn {
        let macro_internal = self.0.struct_attrs().macro_internal_path();

        let fields = self.0.fields();
        let visited_field_declarations = fields.iter().flat_map(Self::generate_visited_flag_decl);
        let type_check_blocks = fields.iter().flat_map(|f| self.generate_type_check(f));
        let append_name_blocks = fields.iter().flat_map(Self::generate_append_name);
        let nonskipped_field_names = fields
            .iter()
            .filter(|f| !f.skip)
            .map(|f| f.cql_name_literal());
        let field_count_lit = fields.iter().filter(|f| f.is_required()).count();

        parse_quote! {
            fn type_check(
                specs: &[#macro_internal::ColumnSpec],
            ) -> ::std::result::Result<(), #macro_internal::TypeCheckError> {
                // Counts down how many required fields are remaining
                let mut remaining_required_fields: ::std::primitive::usize = #field_count_lit;

                // For each required field, generate a "visited" boolean flag
                #(#visited_field_declarations)*

                let column_types_iter = || specs.iter().map(|spec| ::std::clone::Clone::clone(&spec.typ));

                for (column_index, spec) in specs.iter().enumerate() {
                    // Pattern match on the name and verify that the type is correct.
                    match spec.name.as_str() {
                        #(#nonskipped_field_names => #type_check_blocks,)*
                        _unknown => {
                            return ::std::result::Result::Err(
                                #macro_internal::mk_row_typck_err::<Self>(
                                    column_types_iter(),
                                    #macro_internal::DeserBuiltinRowTypeCheckErrorKind::ColumnWithUnknownName {
                                        column_index,
                                        column_name: <_ as ::std::clone::Clone>::clone(&spec.name)
                                    }
                                )
                            )
                        }
                    }
                }

                if remaining_required_fields > 0 {
                    // If there are some missing required fields, generate an error
                    // which contains missing field names
                    let mut missing_fields = ::std::vec::Vec::<&'static str>::with_capacity(remaining_required_fields);
                    #(#append_name_blocks)*
                    return ::std::result::Result::Err(
                        #macro_internal::mk_row_typck_err::<Self>(
                            column_types_iter(),
                            #macro_internal::DeserBuiltinRowTypeCheckErrorKind::ValuesMissingForColumns {
                                column_names: missing_fields
                            }
                        )
                    )
                }

                ::std::result::Result::Ok(())
            }
        }
    }
}

struct DeserializeUnorderedGenerator<'sd>(&'sd StructDesc);

impl<'sd> DeserializeUnorderedGenerator<'sd> {
    // An identifier for a variable that is meant to store the parsed variable
    // before being ultimately moved to the struct on deserialize
    fn deserialize_field_variable(field: &Field) -> syn::Ident {
        quote::format_ident!("f_{}", field.ident.as_ref().unwrap().unraw())
    }

    // Generates an expression which produces a value ready to be put into a field
    // of the target structure
    fn generate_finalize_field(&self, field: &Field) -> syn::Expr {
        if field.skip {
            // Skipped fields are initialized with Default::default()
            return parse_quote! {
                ::std::default::Default::default()
            };
        }

        let deserialize_field = Self::deserialize_field_variable(field);
        let cql_name_literal = field.cql_name_literal();
        parse_quote! {
            #deserialize_field.unwrap_or_else(|| panic!(
                "column {} missing in DB row - type check should have prevented this!",
                #cql_name_literal
            ))
        }
    }

    // Generated code that performs deserialization when the raw field
    // is being processed
    fn generate_deserialization(&self, column_index: usize, field: &Field) -> syn::Expr {
        assert!(!field.skip);
        let macro_internal = self.0.struct_attrs().macro_internal_path();
        let constraint_lifetime = self.0.constraint_lifetime();
        let deserialize_field = Self::deserialize_field_variable(field);
        let deserializer = field.deserialize_target();

        parse_quote! {
            {
                assert!(
                    #deserialize_field.is_none(),
                    "duplicated column {} - type check should have prevented this!",
                    stringify!(#deserialize_field)
                );

                #deserialize_field = ::std::option::Option::Some(
                    <#deserializer as #macro_internal::DeserializeValue<#constraint_lifetime>>::deserialize(&col.spec.typ, col.slice)
                        .map_err(|err| {
                            #macro_internal::mk_row_deser_err::<Self>(
                                #macro_internal::BuiltinRowDeserializationErrorKind::ColumnDeserializationFailed {
                                    column_index: #column_index,
                                    column_name: <_ as std::clone::Clone>::clone(&col.spec.name),
                                    err,
                                }
                            )
                        })?
                );
            }
        }
    }

    // Generate a declaration of a variable that temporarily keeps
    // the deserialized value
    fn generate_deserialize_field_decl(field: &Field) -> Option<syn::Stmt> {
        (!field.skip).then(|| {
            let deserialize_field = Self::deserialize_field_variable(field);
            parse_quote! {
                let mut #deserialize_field = ::std::option::Option::None;
            }
        })
    }

    fn generate(&self) -> syn::ImplItemFn {
        let macro_internal = self.0.struct_attrs().macro_internal_path();
        let constraint_lifetime = self.0.constraint_lifetime();
        let fields = self.0.fields();

        let deserialize_field_decls = fields
            .iter()
            .flat_map(Self::generate_deserialize_field_decl);
        let deserialize_blocks = fields
            .iter()
            .filter(|f| !f.skip)
            .enumerate()
            .map(|(col_idx, f)| self.generate_deserialization(col_idx, f));
        let field_idents = fields.iter().map(|f| f.ident.as_ref().unwrap());
        let nonskipped_field_names = fields
            .iter()
            .filter(|&f| (!f.skip))
            .map(|f| f.cql_name_literal());

        let field_finalizers = fields.iter().map(|f| self.generate_finalize_field(f));

        // TODO: Allow collecting unrecognized fields into some special field

        parse_quote! {
            fn deserialize(
                #[allow(unused_mut)]
                mut row: #macro_internal::ColumnIterator<#constraint_lifetime>,
            ) -> ::std::result::Result<Self, #macro_internal::DeserializationError> {

                // Generate fields that will serve as temporary storage
                // for the fields' values. Those are of type Option<FieldType>.
                #(#deserialize_field_decls)*

                for col in row {
                    let col = col.map_err(#macro_internal::row_deser_error_replace_rust_name::<Self>)?;
                    // Pattern match on the field name and deserialize.
                    match col.spec.name.as_str() {
                        #(#nonskipped_field_names => #deserialize_blocks,)*
                        unknown => unreachable!("Typecheck should have prevented this scenario! Unknown column name: {}", unknown),
                    }
                }

                // Create the final struct. The finalizer expressions convert
                // the temporary storage fields to the final field values.
                // For example, if a field is missing but marked as
                // `default_when_null` it will create a default value, otherwise
                // it will report an error.
                Ok(Self {
                    #(#field_idents: #field_finalizers,)*
                })
            }
        }
    }
}
