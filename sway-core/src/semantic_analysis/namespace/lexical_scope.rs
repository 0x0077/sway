use crate::{
    decl_engine::{parsed_engine::ParsedDeclEngineGet, parsed_id::ParsedDeclId, *},
    engine_threading::{Engines, PartialEqWithEngines, PartialEqWithEnginesContext},
    language::{
        parsed::{Declaration, FunctionDeclaration},
        ty::{self, StructAccessInfo, TyDecl, TyStorageDecl},
        CallPath, Visibility,
    },
    namespace::*,
    semantic_analysis::{ast_node::ConstShadowingMode, GenericShadowingMode},
    type_system::*,
};

use super::{root::ResolvedDeclaration, TraitMap};

use parking_lot::RwLock;
use sway_error::{
    error::{CompileError, ShadowingSource, StructFieldUsageContext},
    handler::{ErrorEmitted, Handler},
};
use sway_types::{span::Span, IdentUnique, Named, Spanned};

use std::sync::Arc;

pub enum ResolvedFunctionDecl {
    Parsed(ParsedDeclId<FunctionDeclaration>),
    Typed(DeclRefFunction),
}

impl ResolvedFunctionDecl {
    pub fn expect_typed(self) -> DeclRefFunction {
        match self {
            ResolvedFunctionDecl::Parsed(_) => panic!(),
            ResolvedFunctionDecl::Typed(fn_ref) => fn_ref,
        }
    }
}

pub(super) type SymbolMap = im::OrdMap<Ident, ResolvedDeclaration>;
pub(super) type SymbolUniqueMap = im::OrdMap<IdentUnique, ResolvedDeclaration>;

type SourceIdent = Ident;

pub(super) type GlobSynonyms =
    im::HashMap<Ident, Vec<(ModulePathBuf, ResolvedDeclaration, Visibility)>>;
pub(super) type ItemSynonyms = im::HashMap<
    Ident,
    (
        Option<SourceIdent>,
        ModulePathBuf,
        ResolvedDeclaration,
        Visibility,
    ),
>;

/// Represents a lexical scope integer-based identifier, which can be used to reference
/// specific a lexical scope.
pub type LexicalScopeId = usize;

/// Represents a lexical scope path, a vector of lexical scope identifiers, which specifies
/// the path from root to a specific lexical scope in the hierarchy.
pub type LexicalScopePath = Vec<LexicalScopeId>;

/// A `LexicalScope` contains a set of all items that exist within the lexical scope via declaration or
/// importing, along with all its associated hierarchical scopes.
#[derive(Clone, Debug, Default)]
pub struct LexicalScope {
    /// The set of symbols, implementations, synonyms and aliases present within this scope.
    pub items: Items,
    /// The set of available scopes defined inside this scope's hierarchy.
    pub children: Vec<LexicalScopeId>,
    /// The parent scope associated with this scope. Will be None for a root scope.
    pub parent: Option<LexicalScopeId>,
}

/// The set of items that exist within some lexical scope via declaration or importing.
#[derive(Clone, Debug, Default)]
pub struct Items {
    /// An ordered map from `Ident`s to their associated declarations.
    pub(crate) symbols: SymbolMap,

    /// An ordered map from `IdentUnique`s to their associated declarations.
    /// This uses an Arc<RwLock<SymbolUniqueMap>> so it is shared between all
    /// Items clones. This is intended so we can keep the symbols of previous
    /// lexical scopes while collecting_unifications scopes.
    pub(crate) symbols_unique_while_collecting_unifications: Arc<RwLock<SymbolUniqueMap>>,

    pub(crate) implemented_traits: TraitMap,
    /// Contains symbols imported using star imports (`use foo::*`.).
    ///
    /// When star importing from multiple modules the same name may be imported more than once. This
    /// is not an error, but it is an error to use the name without a module path. To represent
    /// this, use_glob_synonyms maps identifiers to a vector of (module path, type declaration)
    /// tuples.
    pub(crate) use_glob_synonyms: GlobSynonyms,
    /// Contains symbols imported using item imports (`use foo::bar`).
    ///
    /// For aliased item imports `use ::foo::bar::Baz as Wiz` the map key is `Wiz`. `Baz` is stored
    /// as the optional source identifier for error reporting purposes.
    pub(crate) use_item_synonyms: ItemSynonyms,
    /// If there is a storage declaration (which are only valid in contracts), store it here.
    pub(crate) declared_storage: Option<DeclRefStorage>,
}

impl Items {
    /// Immutable access to the inner symbol map.
    pub fn symbols(&self) -> &SymbolMap {
        &self.symbols
    }

    #[allow(clippy::too_many_arguments)]
    pub fn apply_storage_load(
        &self,
        handler: &Handler,
        engines: &Engines,
        namespace: &Namespace,
        namespace_names: &[Ident],
        fields: &[Ident],
        storage_fields: &[ty::TyStorageField],
        storage_keyword_span: Span,
    ) -> Result<(ty::TyStorageAccess, TypeId), ErrorEmitted> {
        match self.declared_storage {
            Some(ref decl_ref) => {
                let storage = engines.de().get_storage(&decl_ref.id().clone());
                storage.apply_storage_load(
                    handler,
                    engines,
                    namespace,
                    namespace_names,
                    fields,
                    storage_fields,
                    storage_keyword_span,
                )
            }
            None => Err(handler.emit_err(CompileError::NoDeclaredStorage {
                span: fields[0].span(),
            })),
        }
    }

    pub fn set_storage_declaration(
        &mut self,
        handler: &Handler,
        decl_ref: DeclRefStorage,
    ) -> Result<(), ErrorEmitted> {
        if self.declared_storage.is_some() {
            return Err(handler.emit_err(CompileError::MultipleStorageDeclarations {
                span: decl_ref.span(),
            }));
        }
        self.declared_storage = Some(decl_ref);
        Ok(())
    }

    pub fn get_all_declared_symbols(&self) -> impl Iterator<Item = &Ident> {
        self.symbols().keys()
    }

    pub fn resolve_symbol(
        &self,
        handler: &Handler,
        engines: &Engines,
        symbol: &Ident,
    ) -> Result<ResolvedDeclaration, ErrorEmitted> {
        // Check locally declared items. Any name clash with imports will have already been reported as an error.
        if let Some(decl) = self.symbols.get(symbol) {
            return Ok(decl.clone());
        }

        // Check item imports
        if let Some((_, _, decl, _)) = self.use_item_synonyms.get(symbol) {
            return Ok(decl.clone());
        }

        // Check glob imports
        if let Some(decls) = self.use_glob_synonyms.get(symbol) {
            if decls.len() == 1 {
                return Ok(decls[0].1.clone());
            } else if decls.is_empty() {
                return Err(handler.emit_err(CompileError::Internal(
                    "The name {symbol} was bound in a star import, but no corresponding module paths were found",
                    symbol.span(),
                )));
            } else {
                return Err(handler.emit_err(CompileError::SymbolWithMultipleBindings {
                    name: symbol.clone(),
                    paths: decls
                        .iter()
                        .map(|(path, decl, _)| get_path_for_decl(path, decl, engines))
                        .collect(),
                    span: symbol.span(),
                }));
            }
        }

        // Symbol not found
        Err(handler.emit_err(CompileError::SymbolNotFound {
            name: symbol.clone(),
            span: symbol.span(),
        }))
    }

    pub(crate) fn insert_parsed_symbol(
        &mut self,
        handler: &Handler,
        engines: &Engines,
        name: Ident,
        item: Declaration,
        const_shadowing_mode: ConstShadowingMode,
        generic_shadowing_mode: GenericShadowingMode,
    ) -> Result<(), ErrorEmitted> {
        self.insert_symbol(
            handler,
            engines,
            name,
            ResolvedDeclaration::Parsed(item),
            const_shadowing_mode,
            generic_shadowing_mode,
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn insert_typed_symbol(
        &mut self,
        handler: &Handler,
        engines: &Engines,
        name: Ident,
        item: ty::TyDecl,
        const_shadowing_mode: ConstShadowingMode,
        generic_shadowing_mode: GenericShadowingMode,
        collecting_unifications: bool,
    ) -> Result<(), ErrorEmitted> {
        self.insert_symbol(
            handler,
            engines,
            name,
            ResolvedDeclaration::Typed(item),
            const_shadowing_mode,
            generic_shadowing_mode,
            collecting_unifications,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn insert_symbol(
        &mut self,
        handler: &Handler,
        engines: &Engines,
        name: Ident,
        item: ResolvedDeclaration,
        const_shadowing_mode: ConstShadowingMode,
        generic_shadowing_mode: GenericShadowingMode,
        collecting_unifications: bool,
    ) -> Result<(), ErrorEmitted> {
        let parsed_decl_engine = engines.pe();
        let decl_engine = engines.de();

        #[allow(unused)]
        let append_shadowing_error_parsed =
            |ident: &Ident,
             decl: &Declaration,
             is_use: bool,
             is_alias: bool,
             item: &Declaration,
             const_shadowing_mode: ConstShadowingMode| {
                use Declaration::*;
                match (
                    ident,
                    decl,
                    is_use,
                    is_alias,
                    &item,
                    const_shadowing_mode,
                    generic_shadowing_mode,
                ) {
                    // A general remark for using the `ShadowingSource::LetVar`.
                    // If the shadowing is detected at this stage, the variable is for
                    // sure a local variable, because in the case of pattern matching
                    // struct field variables, the error is already reported and
                    // the compilation do not proceed to the point of inserting
                    // the pattern variable into the items.

                    // variable shadowing a constant
                    (
                        constant_ident,
                        ConstantDeclaration(decl_id),
                        is_imported_constant,
                        is_alias,
                        VariableDeclaration { .. },
                        _,
                        _,
                    ) => {
                        handler.emit_err(CompileError::ConstantsCannotBeShadowed {
                            shadowing_source: ShadowingSource::LetVar,
                            name: (&name).into(),
                            constant_span: constant_ident.span(),
                            constant_decl_span: if is_imported_constant {
                                parsed_decl_engine.get(decl_id).span.clone()
                            } else {
                                Span::dummy()
                            },
                            is_alias,
                        });
                    }
                    // variable shadowing a configurable
                    (
                        configurable_ident,
                        ConfigurableDeclaration(_),
                        _,
                        _,
                        VariableDeclaration { .. },
                        _,
                        _,
                    ) => {
                        handler.emit_err(CompileError::ConfigurablesCannotBeShadowed {
                            shadowing_source: ShadowingSource::LetVar,
                            name: (&name).into(),
                            configurable_span: configurable_ident.span(),
                        });
                    }
                    // constant shadowing a constant sequentially
                    (
                        constant_ident,
                        ConstantDeclaration(decl_id),
                        is_imported_constant,
                        is_alias,
                        ConstantDeclaration { .. },
                        ConstShadowingMode::Sequential,
                        _,
                    ) => {
                        handler.emit_err(CompileError::ConstantsCannotBeShadowed {
                            shadowing_source: ShadowingSource::Const,
                            name: (&name).into(),
                            constant_span: constant_ident.span(),
                            constant_decl_span: if is_imported_constant {
                                parsed_decl_engine.get(decl_id).span.clone()
                            } else {
                                Span::dummy()
                            },
                            is_alias,
                        });
                    }
                    // constant shadowing a configurable sequentially
                    (
                        configurable_ident,
                        ConfigurableDeclaration(_),
                        _,
                        _,
                        ConstantDeclaration { .. },
                        ConstShadowingMode::Sequential,
                        _,
                    ) => {
                        handler.emit_err(CompileError::ConfigurablesCannotBeShadowed {
                            shadowing_source: ShadowingSource::Const,
                            name: (&name).into(),
                            configurable_span: configurable_ident.span(),
                        });
                    }
                    // constant shadowing a variable
                    (_, VariableDeclaration(decl_id), _, _, ConstantDeclaration { .. }, _, _) => {
                        handler.emit_err(CompileError::ConstantShadowsVariable {
                            name: (&name).into(),
                            variable_span: parsed_decl_engine.get(decl_id).name.span(),
                        });
                    }
                    // constant shadowing a constant item-style (outside of a function body)
                    (
                        constant_ident,
                        ConstantDeclaration { .. },
                        _,
                        _,
                        ConstantDeclaration { .. },
                        ConstShadowingMode::ItemStyle,
                        _,
                    ) => {
                        handler.emit_err(CompileError::ConstantDuplicatesConstantOrConfigurable {
                            existing_constant_or_configurable: "Constant",
                            new_constant_or_configurable: "Constant",
                            name: (&name).into(),
                            existing_span: constant_ident.span(),
                        });
                    }
                    // constant shadowing a configurable item-style (outside of a function body)
                    (
                        configurable_ident,
                        ConfigurableDeclaration { .. },
                        _,
                        _,
                        ConstantDeclaration { .. },
                        ConstShadowingMode::ItemStyle,
                        _,
                    ) => {
                        handler.emit_err(CompileError::ConstantDuplicatesConstantOrConfigurable {
                            existing_constant_or_configurable: "Configurable",
                            new_constant_or_configurable: "Constant",
                            name: (&name).into(),
                            existing_span: configurable_ident.span(),
                        });
                    }
                    // configurable shadowing a constant item-style (outside of a function body)
                    (
                        constant_ident,
                        ConstantDeclaration { .. },
                        _,
                        _,
                        ConfigurableDeclaration { .. },
                        ConstShadowingMode::ItemStyle,
                        _,
                    ) => {
                        handler.emit_err(CompileError::ConstantDuplicatesConstantOrConfigurable {
                            existing_constant_or_configurable: "Constant",
                            new_constant_or_configurable: "Configurable",
                            name: (&name).into(),
                            existing_span: constant_ident.span(),
                        });
                    }
                    // type or type alias shadowing another type or type alias
                    // trait/abi shadowing another trait/abi
                    // type or type alias shadowing a trait/abi, or vice versa
                    (
                        _,
                        StructDeclaration { .. }
                        | EnumDeclaration { .. }
                        | TypeAliasDeclaration { .. }
                        | TraitDeclaration { .. }
                        | AbiDeclaration { .. },
                        _,
                        _,
                        StructDeclaration { .. }
                        | EnumDeclaration { .. }
                        | TypeAliasDeclaration { .. }
                        | TraitDeclaration { .. }
                        | AbiDeclaration { .. },
                        _,
                        _,
                    ) => {
                        handler.emit_err(CompileError::MultipleDefinitionsOfName {
                            name: name.clone(),
                            span: name.span(),
                        });
                    }
                    _ => {}
                }
            };

        let append_shadowing_error_typed =
            |ident: &Ident,
             decl: &ty::TyDecl,
             is_use: bool,
             is_alias: bool,
             item: &ty::TyDecl,
             const_shadowing_mode: ConstShadowingMode| {
                use ty::TyDecl::*;
                match (
                    ident,
                    decl,
                    is_use,
                    is_alias,
                    &item,
                    const_shadowing_mode,
                    generic_shadowing_mode,
                ) {
                    // A general remark for using the `ShadowingSource::LetVar`.
                    // If the shadowing is detected at this stage, the variable is for
                    // sure a local variable, because in the case of pattern matching
                    // struct field variables, the error is already reported and
                    // the compilation do not proceed to the point of inserting
                    // the pattern variable into the items.

                    // variable shadowing a constant
                    (
                        constant_ident,
                        ConstantDecl(constant_decl),
                        is_imported_constant,
                        is_alias,
                        VariableDecl { .. },
                        _,
                        _,
                    ) => {
                        handler.emit_err(CompileError::ConstantsCannotBeShadowed {
                            shadowing_source: ShadowingSource::LetVar,
                            name: (&name).into(),
                            constant_span: constant_ident.span(),
                            constant_decl_span: if is_imported_constant {
                                decl_engine.get(&constant_decl.decl_id).span.clone()
                            } else {
                                Span::dummy()
                            },
                            is_alias,
                        });
                    }
                    // variable shadowing a configurable
                    (configurable_ident, ConfigurableDecl(_), _, _, VariableDecl { .. }, _, _) => {
                        handler.emit_err(CompileError::ConfigurablesCannotBeShadowed {
                            shadowing_source: ShadowingSource::LetVar,
                            name: (&name).into(),
                            configurable_span: configurable_ident.span(),
                        });
                    }
                    // constant shadowing a constant sequentially
                    (
                        constant_ident,
                        ConstantDecl(constant_decl),
                        is_imported_constant,
                        is_alias,
                        ConstantDecl { .. },
                        ConstShadowingMode::Sequential,
                        _,
                    ) => {
                        handler.emit_err(CompileError::ConstantsCannotBeShadowed {
                            shadowing_source: ShadowingSource::Const,
                            name: (&name).into(),
                            constant_span: constant_ident.span(),
                            constant_decl_span: if is_imported_constant {
                                decl_engine.get(&constant_decl.decl_id).span.clone()
                            } else {
                                Span::dummy()
                            },
                            is_alias,
                        });
                    }
                    // constant shadowing a configurable sequentially
                    (
                        configurable_ident,
                        ConfigurableDecl(_),
                        _,
                        _,
                        ConstantDecl { .. },
                        ConstShadowingMode::Sequential,
                        _,
                    ) => {
                        handler.emit_err(CompileError::ConfigurablesCannotBeShadowed {
                            shadowing_source: ShadowingSource::Const,
                            name: (&name).into(),
                            configurable_span: configurable_ident.span(),
                        });
                    }
                    // constant shadowing a variable
                    (_, VariableDecl(variable_decl), _, _, ConstantDecl { .. }, _, _) => {
                        handler.emit_err(CompileError::ConstantShadowsVariable {
                            name: (&name).into(),
                            variable_span: variable_decl.name.span(),
                        });
                    }
                    // constant shadowing a constant item-style (outside of a function body)
                    (
                        constant_ident,
                        ConstantDecl { .. },
                        _,
                        _,
                        ConstantDecl { .. },
                        ConstShadowingMode::ItemStyle,
                        _,
                    ) => {
                        handler.emit_err(CompileError::ConstantDuplicatesConstantOrConfigurable {
                            existing_constant_or_configurable: "Constant",
                            new_constant_or_configurable: "Constant",
                            name: (&name).into(),
                            existing_span: constant_ident.span(),
                        });
                    }
                    // constant shadowing a configurable item-style (outside of a function body)
                    (
                        configurable_ident,
                        ConfigurableDecl { .. },
                        _,
                        _,
                        ConstantDecl { .. },
                        ConstShadowingMode::ItemStyle,
                        _,
                    ) => {
                        handler.emit_err(CompileError::ConstantDuplicatesConstantOrConfigurable {
                            existing_constant_or_configurable: "Configurable",
                            new_constant_or_configurable: "Constant",
                            name: (&name).into(),
                            existing_span: configurable_ident.span(),
                        });
                    }
                    // configurable shadowing a constant item-style (outside of a function body)
                    (
                        constant_ident,
                        ConstantDecl { .. },
                        _,
                        _,
                        ConfigurableDecl { .. },
                        ConstShadowingMode::ItemStyle,
                        _,
                    ) => {
                        handler.emit_err(CompileError::ConstantDuplicatesConstantOrConfigurable {
                            existing_constant_or_configurable: "Constant",
                            new_constant_or_configurable: "Configurable",
                            name: (&name).into(),
                            existing_span: constant_ident.span(),
                        });
                    }
                    // type or type alias shadowing another type or type alias
                    // trait/abi shadowing another trait/abi
                    // type or type alias shadowing a trait/abi, or vice versa
                    (
                        _,
                        StructDecl { .. }
                        | EnumDecl { .. }
                        | TypeAliasDecl { .. }
                        | TraitDecl { .. }
                        | AbiDecl { .. },
                        _,
                        _,
                        StructDecl { .. }
                        | EnumDecl { .. }
                        | TypeAliasDecl { .. }
                        | TraitDecl { .. }
                        | AbiDecl { .. },
                        _,
                        _,
                    ) => {
                        handler.emit_err(CompileError::MultipleDefinitionsOfName {
                            name: name.clone(),
                            span: name.span(),
                        });
                    }
                    // generic parameter shadowing another generic parameter
                    (
                        _,
                        GenericTypeForFunctionScope { .. },
                        _,
                        _,
                        GenericTypeForFunctionScope { .. },
                        _,
                        GenericShadowingMode::Disallow,
                    ) => {
                        handler.emit_err(CompileError::GenericShadowsGeneric {
                            name: (&name).into(),
                        });
                    }
                    _ => {}
                }
            };

        let append_shadowing_error =
            |ident: &Ident,
             decl: &ResolvedDeclaration,
             is_use: bool,
             is_alias: bool,
             item: &ResolvedDeclaration,
             const_shadowing_mode: ConstShadowingMode| {
                match (decl, item) {
                    // TODO: Do not handle any shadowing errors while handling parsed declarations yet,
                    // or else we will emit errors in a different order from the source code order.
                    // Update this once the full AST resolving pass is in.
                    (ResolvedDeclaration::Typed(_decl), ResolvedDeclaration::Parsed(_item)) => {}
                    (ResolvedDeclaration::Parsed(_decl), ResolvedDeclaration::Parsed(_item)) => {}
                    (ResolvedDeclaration::Typed(decl), ResolvedDeclaration::Typed(item)) => {
                        append_shadowing_error_typed(
                            ident,
                            decl,
                            is_use,
                            is_alias,
                            item,
                            const_shadowing_mode,
                        )
                    }
                    _ => unreachable!(),
                }
            };

        if let Some((ident, decl)) = self.symbols.get_key_value(&name) {
            append_shadowing_error(
                ident,
                decl,
                false,
                false,
                &item.clone(),
                const_shadowing_mode,
            );
        }

        if let Some((ident, (imported_ident, _, decl, _))) =
            self.use_item_synonyms.get_key_value(&name)
        {
            append_shadowing_error(
                ident,
                decl,
                true,
                imported_ident.is_some(),
                &item,
                const_shadowing_mode,
            );
        }

        if collecting_unifications {
            self.symbols_unique_while_collecting_unifications
                .write()
                .insert(name.clone().into(), item.clone());
        }
        self.symbols.insert(name, item);

        Ok(())
    }

    // Add a new binding into use_glob_synonyms. The symbol may already be bound by an earlier
    // insertion, in which case the new binding is added as well so that multiple bindings exist.
    //
    // There are a few edge cases were a new binding will replace an old binding. These edge cases
    // are a consequence of the prelude reexports not being implemented properly. See comments in
    // the code for details.
    pub(crate) fn insert_glob_use_symbol(
        &mut self,
        engines: &Engines,
        symbol: Ident,
        src_path: ModulePathBuf,
        decl: &ResolvedDeclaration,
        visibility: Visibility,
    ) {
        if let Some(cur_decls) = self.use_glob_synonyms.get_mut(&symbol) {
            // Name already bound. Check if the decl is already imported
            let ctx = PartialEqWithEnginesContext::new(engines);
            match cur_decls
                .iter()
                .position(|(_cur_path, cur_decl, _cur_visibility)| cur_decl.eq(decl, &ctx))
            {
                Some(index) if matches!(visibility, Visibility::Public) => {
                    // The name is already bound to this decl. If the new symbol is more visible
                    // than the old one, then replace the old one.
                    cur_decls[index] = (src_path.to_vec(), decl.clone(), visibility);
                }
                Some(_) => {
                    // Same binding as the existing one. Do nothing.
                }
                None => {
                    // New decl for this name. Add it to the end
                    cur_decls.push((src_path.to_vec(), decl.clone(), visibility));
                }
            }
        } else {
            let new_vec = vec![(src_path.to_vec(), decl.clone(), visibility)];
            self.use_glob_synonyms.insert(symbol, new_vec);
        }
    }

    pub(crate) fn check_symbol(&self, name: &Ident) -> Result<ResolvedDeclaration, CompileError> {
        self.symbols
            .get(name)
            .cloned()
            .ok_or_else(|| CompileError::SymbolNotFound {
                name: name.clone(),
                span: name.span(),
            })
    }

    pub(crate) fn check_symbols_unique_while_collecting_unifications(
        &self,
        name: &Ident,
    ) -> Result<ResolvedDeclaration, CompileError> {
        self.symbols_unique_while_collecting_unifications
            .read()
            .get(&name.into())
            .cloned()
            .ok_or_else(|| CompileError::SymbolNotFound {
                name: name.clone(),
                span: name.span(),
            })
    }

    pub(crate) fn clear_symbols_unique_while_collecting_unifications(&self) {
        self.symbols_unique_while_collecting_unifications
            .write()
            .clear();
    }

    pub fn get_items_for_type(
        &self,
        engines: &Engines,
        type_id: TypeId,
    ) -> Vec<ResolvedTraitImplItem> {
        self.implemented_traits.get_items_for_type(engines, type_id)
    }

    pub fn get_impl_spans_for_decl(&self, engines: &Engines, ty_decl: &TyDecl) -> Vec<Span> {
        let handler = Handler::default();
        ty_decl
            .return_type(&handler, engines)
            .map(|type_id| {
                self.implemented_traits
                    .get_impl_spans_for_type(engines, &type_id)
            })
            .unwrap_or_default()
    }

    pub fn get_impl_spans_for_type(&self, engines: &Engines, type_id: &TypeId) -> Vec<Span> {
        self.implemented_traits
            .get_impl_spans_for_type(engines, type_id)
    }

    pub fn get_impl_spans_for_trait_name(&self, trait_name: &CallPath) -> Vec<Span> {
        self.implemented_traits
            .get_impl_spans_for_trait_name(trait_name)
    }

    pub fn get_methods_for_type(
        &self,
        engines: &Engines,
        type_id: TypeId,
    ) -> Vec<ResolvedFunctionDecl> {
        self.get_items_for_type(engines, type_id)
            .into_iter()
            .filter_map(|item| match item {
                ResolvedTraitImplItem::Parsed(_) => todo!(),
                ResolvedTraitImplItem::Typed(item) => match item {
                    ty::TyTraitItem::Fn(decl_ref) => Some(ResolvedFunctionDecl::Typed(decl_ref)),
                    ty::TyTraitItem::Constant(_decl_ref) => None,
                    ty::TyTraitItem::Type(_decl_ref) => None,
                },
            })
            .collect::<Vec<_>>()
    }

    pub(crate) fn has_storage_declared(&self) -> bool {
        self.declared_storage.is_some()
    }

    pub fn get_declared_storage(&self, decl_engine: &DeclEngine) -> Option<TyStorageDecl> {
        self.declared_storage
            .as_ref()
            .map(|decl_ref| (*decl_engine.get_storage(decl_ref)).clone())
    }

    pub(crate) fn get_storage_field_descriptors(
        &self,
        handler: &Handler,
        decl_engine: &DeclEngine,
    ) -> Result<Vec<ty::TyStorageField>, ErrorEmitted> {
        match self.get_declared_storage(decl_engine) {
            Some(storage) => Ok(storage.fields.clone()),
            None => {
                let msg = "unknown source location";
                let span = Span::new(Arc::from(msg), 0, msg.len(), None).unwrap();
                Err(handler.emit_err(CompileError::NoDeclaredStorage { span }))
            }
        }
    }

    /// Returns a tuple where the first element is the [TypeId] of the actual expression, and
    /// the second is the [TypeId] of its parent.
    pub(crate) fn find_subfield_type(
        &self,
        handler: &Handler,
        engines: &Engines,
        namespace: &Namespace,
        base_name: &Ident,
        projections: &[ty::ProjectionKind],
    ) -> Result<(TypeId, TypeId), ErrorEmitted> {
        let type_engine = engines.te();
        let decl_engine = engines.de();

        let symbol = match self.symbols.get(base_name).cloned() {
            Some(s) => s,
            None => {
                return Err(handler.emit_err(CompileError::UnknownVariable {
                    var_name: base_name.clone(),
                    span: base_name.span(),
                }));
            }
        };
        let mut symbol = match symbol {
            ResolvedDeclaration::Parsed(_) => unreachable!(),
            ResolvedDeclaration::Typed(ty_decl) => ty_decl.return_type(handler, engines)?,
        };
        let mut symbol_span = base_name.span();
        let mut parent_rover = symbol;
        let mut full_span_for_error = base_name.span();
        for projection in projections {
            let resolved_type = match type_engine.to_typeinfo(symbol, &symbol_span) {
                Ok(resolved_type) => resolved_type,
                Err(error) => {
                    return Err(handler.emit_err(CompileError::TypeError(error)));
                }
            };
            match (resolved_type, projection) {
                (
                    TypeInfo::Struct(decl_ref),
                    ty::ProjectionKind::StructField { name: field_name },
                ) => {
                    let struct_decl = decl_engine.get_struct(&decl_ref);
                    let (struct_can_be_changed, is_public_struct_access) =
                        StructAccessInfo::get_info(engines, &struct_decl, namespace).into();

                    let field_type_id = match struct_decl.find_field(field_name) {
                        Some(struct_field) => {
                            if is_public_struct_access && struct_field.is_private() {
                                return Err(handler.emit_err(CompileError::StructFieldIsPrivate {
                                    field_name: field_name.into(),
                                    struct_name: struct_decl.call_path.suffix.clone(),
                                    field_decl_span: struct_field.name.span(),
                                    struct_can_be_changed,
                                    usage_context: StructFieldUsageContext::StructFieldAccess,
                                }));
                            }

                            struct_field.type_argument.type_id
                        }
                        None => {
                            return Err(handler.emit_err(CompileError::StructFieldDoesNotExist {
                                field_name: field_name.into(),
                                available_fields: struct_decl
                                    .accessible_fields_names(is_public_struct_access),
                                is_public_struct_access,
                                struct_name: struct_decl.call_path.suffix.clone(),
                                struct_decl_span: struct_decl.span(),
                                struct_is_empty: struct_decl.is_empty(),
                                usage_context: StructFieldUsageContext::StructFieldAccess,
                            }));
                        }
                    };
                    parent_rover = symbol;
                    symbol = field_type_id;
                    symbol_span = field_name.span().clone();
                    full_span_for_error = Span::join(full_span_for_error, &field_name.span());
                }
                (TypeInfo::Tuple(fields), ty::ProjectionKind::TupleField { index, index_span }) => {
                    let field_type_opt = {
                        fields
                            .get(*index)
                            .map(|TypeArgument { type_id, .. }| type_id)
                    };
                    let field_type = match field_type_opt {
                        Some(field_type) => field_type,
                        None => {
                            return Err(handler.emit_err(CompileError::TupleIndexOutOfBounds {
                                index: *index,
                                count: fields.len(),
                                tuple_type: engines.help_out(symbol).to_string(),
                                span: index_span.clone(),
                                prefix_span: full_span_for_error.clone(),
                            }));
                        }
                    };
                    parent_rover = symbol;
                    symbol = *field_type;
                    symbol_span = index_span.clone();
                    full_span_for_error = Span::join(full_span_for_error, index_span);
                }
                (
                    TypeInfo::Array(elem_ty, _),
                    ty::ProjectionKind::ArrayIndex { index_span, .. },
                ) => {
                    parent_rover = symbol;
                    symbol = elem_ty.type_id;
                    symbol_span = index_span.clone();
                    // `index_span` does not contain the enclosing square brackets.
                    // Which means, if this array index access is the last one before the
                    // erroneous expression, the `full_span_for_error` will be missing the
                    // closing `]`. We can live with this small glitch so far. To fix it,
                    // we would need to bring the full span of the index all the way from
                    // the parsing stage. An effort that doesn't pay off at the moment.
                    // TODO: Include the closing square bracket into the error span.
                    full_span_for_error = Span::join(full_span_for_error, index_span);
                }
                (actually, ty::ProjectionKind::StructField { name }) => {
                    return Err(handler.emit_err(CompileError::FieldAccessOnNonStruct {
                        actually: engines.help_out(actually).to_string(),
                        storage_variable: None,
                        field_name: name.into(),
                        span: full_span_for_error,
                    }));
                }
                (
                    actually,
                    ty::ProjectionKind::TupleField {
                        index, index_span, ..
                    },
                ) => {
                    return Err(
                        handler.emit_err(CompileError::TupleElementAccessOnNonTuple {
                            actually: engines.help_out(actually).to_string(),
                            span: full_span_for_error,
                            index: *index,
                            index_span: index_span.clone(),
                        }),
                    );
                }
                (actually, ty::ProjectionKind::ArrayIndex { .. }) => {
                    return Err(handler.emit_err(CompileError::NotIndexable {
                        actually: engines.help_out(actually).to_string(),
                        span: full_span_for_error,
                    }));
                }
            }
        }
        Ok((symbol, parent_rover))
    }
}

fn get_path_for_decl(
    path: &[sway_types::BaseIdent],
    decl: &ResolvedDeclaration,
    engines: &Engines,
) -> String {
    let mut path_names = path.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    // Add the enum name to the path if the decl is an enum variant.
    match decl {
        ResolvedDeclaration::Parsed(decl) => {
            if let Declaration::EnumVariantDeclaration(decl) = decl {
                let enum_decl = engines.pe().get_enum(&decl.enum_ref);
                path_names.push(enum_decl.name().to_string())
            };
        }
        ResolvedDeclaration::Typed(decl) => {
            if let TyDecl::EnumVariantDecl(ty::EnumVariantDecl { enum_ref, .. }) = decl {
                path_names.push(enum_ref.name().to_string())
            };
        }
    }
    path_names.join("::")
}
