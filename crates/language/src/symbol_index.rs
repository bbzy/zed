use crate::{BufferSnapshot, Node};
use std::{
    cell::OnceCell,
    collections::{BTreeMap, BTreeSet},
    ops::Range,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymbolLanguage {
    Cpp,
    Java,
    Kotlin,
}

impl SymbolLanguage {
    pub fn for_name(name: &str) -> Option<Self> {
        match name {
            "C++" => Some(Self::Cpp),
            "Java" => Some(Self::Java),
            "Kotlin" => Some(Self::Kotlin),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Namespace,
    Class,
    Interface,
    Enum,
    TypeAlias,
    Function,
    Method,
    Field,
    Variable,
    Parameter,
}

impl Kind {
    pub fn is_type(self) -> bool {
        matches!(
            self,
            Self::Class | Self::Interface | Self::Enum | Self::TypeAlias
        )
    }

    pub fn is_callable(self) -> bool {
        matches!(self, Self::Function | Self::Method)
    }

    pub fn completion_kind(self) -> lsp::CompletionItemKind {
        match self {
            Self::Namespace => lsp::CompletionItemKind::MODULE,
            Self::Class | Self::TypeAlias => lsp::CompletionItemKind::CLASS,
            Self::Interface => lsp::CompletionItemKind::INTERFACE,
            Self::Enum => lsp::CompletionItemKind::ENUM,
            Self::Function => lsp::CompletionItemKind::FUNCTION,
            Self::Method => lsp::CompletionItemKind::METHOD,
            Self::Field => lsp::CompletionItemKind::FIELD,
            Self::Variable | Self::Parameter => lsp::CompletionItemKind::VARIABLE,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Symbol {
    pub name: String,
    pub kind: Kind,
    pub range: Range<usize>,
    pub scope: usize,
    pub owner: String,
    pub type_name: String,
    pub signature: Option<String>,
    pub arity: Option<usize>,
    pub definition: bool,
    pub local: bool,
    pub private: bool,
    pub is_static: bool,
    pub file_local: bool,
}

#[derive(Clone, Debug)]
pub struct Occurrence {
    pub name: String,
    pub range: Range<usize>,
    pub scope: usize,
    pub receiver: Option<String>,
    pub arity: Option<usize>,
    pub type_position: bool,
}

#[derive(Debug)]
pub struct Scope {
    pub range: Range<usize>,
    pub parent: Option<usize>,
    pub owner: String,
    pub callable: bool,
}

#[derive(Debug)]
pub struct Import {
    pub path: String,
    pub alias: String,
    pub wildcard: bool,
}

#[derive(Debug)]
pub struct Document {
    pub language: SymbolLanguage,
    pub symbols: Vec<Symbol>,
    pub occurrences: Vec<Occurrence>,
    pub scopes: Vec<Scope>,
    pub imports: Vec<Import>,
    pub package: String,
    pub path: Option<String>,
    pub includes: Vec<String>,
    pub bases: Vec<(String, String)>,
    pub excluded: Vec<Range<usize>>,
    pub text: Arc<str>,
    declaration_ranges: BTreeSet<(usize, usize)>,
    type_owners: BTreeSet<String>,
}

impl BufferSnapshot {
    pub fn symbol_index(&self) -> Option<Document> {
        if !self.syntax.is_parsed_for_version(self.version()) {
            return None;
        }
        let language = self.language()?;
        let language_kind = SymbolLanguage::for_name(language.name().as_ref())?;
        let layer = self.syntax_layers().find(|layer| {
            layer.language.id() == language.id() && layer.node().byte_range() == (0..self.len())
        })?;
        Some(Document::extract(
            language_kind,
            self.text().into(),
            layer.node(),
        ))
    }
}

impl Document {
    pub fn extract(language: SymbolLanguage, text: Arc<str>, root: Node<'_>) -> Self {
        let mut document = Self {
            language,
            symbols: Vec::new(),
            occurrences: Vec::new(),
            scopes: vec![Scope {
                range: 0..text.len(),
                parent: None,
                owner: String::new(),
                callable: false,
            }],
            imports: Vec::new(),
            package: String::new(),
            path: None,
            includes: Vec::new(),
            bases: Vec::new(),
            excluded: Vec::new(),
            text,
            declaration_ranges: BTreeSet::new(),
            type_owners: BTreeSet::new(),
        };
        document.visit(root, 0, 0);
        document.occurrences.retain(|occurrence| {
            !document
                .declaration_ranges
                .contains(&(occurrence.range.start, occurrence.range.end))
        });
        document
    }

    pub fn memory_cost(&self) -> usize {
        self.text.len()
            + self
                .symbols
                .iter()
                .map(|symbol| {
                    std::mem::size_of::<Symbol>()
                        + symbol.name.len()
                        + symbol.owner.len()
                        + symbol.type_name.len()
                        + symbol.signature.as_ref().map_or(0, String::len)
                })
                .sum::<usize>()
            + self
                .occurrences
                .iter()
                .map(|occurrence| {
                    std::mem::size_of::<Occurrence>()
                        + occurrence.name.len()
                        + occurrence.receiver.as_ref().map_or(0, String::len)
                })
                .sum::<usize>()
            + self.scopes.len() * std::mem::size_of::<Scope>()
            + self.declaration_ranges.len() * 32
    }

    pub fn scope_at(&self, offset: usize) -> usize {
        self.scopes
            .iter()
            .enumerate()
            .rev()
            .find(|(_, scope)| scope.range.start <= offset && offset <= scope.range.end)
            .map_or(0, |(index, _)| index)
    }

    pub fn occurrence_at(&self, offset: usize) -> Option<Occurrence> {
        if let Some(symbol) = self
            .symbols
            .iter()
            .find(|symbol| symbol.range.start <= offset && offset <= symbol.range.end)
        {
            return Some(Occurrence {
                name: symbol.name.clone(),
                range: symbol.range.clone(),
                scope: symbol.scope,
                receiver: (!symbol.owner.is_empty()).then(|| symbol.owner.clone()),
                arity: symbol.arity,
                type_position: symbol.kind.is_type(),
            });
        }
        self.occurrences
            .iter()
            .find(|occurrence| occurrence.range.start <= offset && offset <= occurrence.range.end)
            .cloned()
    }

    pub fn completion_context(&self, offset: usize) -> Option<(Range<usize>, Occurrence)> {
        if !self.text.is_char_boundary(offset)
            || self
                .excluded
                .iter()
                .any(|range| range.start <= offset && offset <= range.end)
        {
            return None;
        }
        let prefix = self.text.get(..offset)?;
        let start = prefix
            .char_indices()
            .rev()
            .take_while(|(_, character)| identifier_character(*character))
            .last()
            .map_or(offset, |(index, _)| index);
        let before = prefix.get(..start)?.trim_end();
        let end = self
            .text
            .get(offset..)?
            .char_indices()
            .take_while(|(_, character)| identifier_character(*character))
            .last()
            .map_or(offset, |(index, character)| {
                offset + index + character.len_utf8()
            });
        let receiver = ["->", "::", "."].iter().find_map(|separator| {
            let before = before.strip_suffix(separator)?.trim_end();
            let receiver = before
                .char_indices()
                .rev()
                .take_while(|(_, character)| {
                    identifier_character(*character) || *character == ':' || *character == '.'
                })
                .last()
                .map(|(index, _)| &before[index..])?;
            Some(normalize(receiver))
        });
        Some((
            start..end,
            Occurrence {
                name: prefix.get(start..)?.to_owned(),
                range: start..offset,
                scope: self.scope_at(offset),
                receiver,
                arity: None,
                type_position: false,
            },
        ))
    }

    fn source(&self, node: Node<'_>) -> &str {
        self.text.get(node.byte_range()).unwrap_or_default()
    }

    fn visit(&mut self, node: Node<'_>, scope: usize, depth: usize) {
        if depth > 256 {
            return;
        }
        let node_kind = node.kind();
        if node_kind == "preproc_include" {
            if let Some(path) = node.child_by_field_name("path") {
                self.includes
                    .push(self.source(path).trim_matches(['"', '<', '>']).to_owned());
            }
            self.excluded.push(node.byte_range());
            return;
        }
        if node_kind.contains("comment")
            || node_kind.contains("string")
            || matches!(node_kind, "character_literal" | "char_literal")
        {
            self.excluded.push(node.byte_range());
            return;
        }
        if matches!(node_kind, "package_declaration" | "package_header") {
            self.package = self
                .source(node)
                .trim()
                .trim_start_matches("package")
                .trim()
                .trim_end_matches(';')
                .to_owned();
            return;
        }
        if matches!(
            node_kind,
            "import_declaration" | "import_header" | "using_declaration"
        ) {
            let source = self.source(node).trim().trim_end_matches(';');
            let source = source
                .strip_prefix("import")
                .or_else(|| source.strip_prefix("using"))
                .unwrap_or(source)
                .trim();
            let source = source.strip_prefix("static ").unwrap_or(source);
            let namespace_import = source.starts_with("namespace ");
            let source = source.strip_prefix("namespace ").unwrap_or(source);
            let (path, alias) = source.split_once(" as ").unwrap_or((source, ""));
            let path = normalize(path);
            let wildcard = namespace_import || path.ends_with("::*");
            let path = path.trim_end_matches("::*").to_owned();
            let alias = if alias.is_empty() {
                terminal(&path).to_owned()
            } else {
                alias.trim().to_owned()
            };
            self.imports.push(Import {
                path,
                alias,
                wildcard,
            });
            return;
        }

        let mut next_scope = scope;
        let mut owner = self.scopes[scope].owner.clone();
        let type_kind = match node_kind {
            "namespace_definition" => Some(Kind::Namespace),
            "class_specifier" | "struct_specifier" | "union_specifier" | "class_declaration"
            | "object_declaration" | "record_declaration" => Some(Kind::Class),
            "interface_declaration" => Some(Kind::Interface),
            "enum_specifier" | "enum_declaration" => Some(Kind::Enum),
            _ => None,
        };
        if let Some(mut kind) = type_kind {
            if self.language == SymbolLanguage::Kotlin
                && self.source(node).trim_start().starts_with("interface ")
            {
                kind = Kind::Interface;
            }
            if let Some(name) = name_node(node) {
                let name_text = self.source(name).to_owned();
                self.add_symbol(name, kind, scope, String::new(), None, true, None);
                owner = qualify(&owner, &name_text);
                for child in named_children(node).filter(|child| {
                    matches!(
                        child.kind(),
                        "base_class_clause"
                            | "superclass"
                            | "super_interfaces"
                            | "delegation_specifiers"
                            | "delegation_specifier"
                    )
                }) {
                    self.collect_bases(child, &owner, 0);
                }
            }
        }

        let callable = matches!(
            node_kind,
            "function_definition"
                | "method_declaration"
                | "constructor_declaration"
                | "function_declaration"
                | "secondary_constructor"
        ) || (matches!(node_kind, "declaration" | "field_declaration")
            && named_children(node)
                .any(|child| find_declarator(child, "function_declarator").is_some()));
        if callable {
            let declarator = node
                .child_by_field_name("declarator")
                .and_then(|declarator| find_declarator(declarator, "function_declarator"));
            let name = declarator
                .and_then(|declarator| declarator.child_by_field_name("declarator"))
                .and_then(declarator_name)
                .or_else(|| name_node(node));
            if let Some(name) = name {
                let parameters = declarator
                    .and_then(|declarator| declarator.child_by_field_name("parameters"))
                    .or_else(|| node.child_by_field_name("parameters"))
                    .or_else(|| {
                        named_children(node).find(|child| {
                            matches!(
                                child.kind(),
                                "function_value_parameters" | "formal_parameters"
                            )
                        })
                    });
                let parameter_types: Vec<_> = parameters
                    .into_iter()
                    .flat_map(named_children)
                    .filter(|child| {
                        matches!(
                            child.kind(),
                            "parameter_declaration"
                                | "optional_parameter_declaration"
                                | "formal_parameter"
                                | "spread_parameter"
                                | "parameter"
                                | "function_value_parameter"
                        )
                    })
                    .map(|parameter| self.type_for(parameter))
                    .collect();
                let body = node.child_by_field_name("body").or_else(|| {
                    named_children(node).find(|child| {
                        matches!(
                            child.kind(),
                            "function_body" | "block" | "compound_statement" | "constructor_body"
                        )
                    })
                });
                let mut kind = if self.type_owners.contains(&owner) {
                    Kind::Method
                } else {
                    Kind::Function
                };
                let explicit_owner = name
                    .parent()
                    .filter(|parent| {
                        matches!(parent.kind(), "qualified_identifier" | "scoped_identifier")
                    })
                    .and_then(|parent| {
                        self.source(parent)
                            .rsplit_once("::")
                            .map(|(owner, _)| normalize(owner))
                    });
                if let Some(explicit_owner) = explicit_owner {
                    owner = explicit_owner;
                    kind = Kind::Method;
                }
                let extension = if self.language == SymbolLanguage::Kotlin {
                    named_children(node)
                        .take_while(|child| child.start_byte() < name.start_byte())
                        .find(|child| matches!(child.kind(), "user_type" | "receiver_type"))
                        .map(|child| normalize(self.source(child)))
                } else {
                    None
                };
                if let Some(extension) = extension {
                    owner = extension;
                }
                let type_name = self.type_for(node);
                self.add_symbol(
                    name,
                    kind,
                    scope,
                    type_name,
                    Some(parameter_types.join(",")),
                    body.is_some(),
                    Some(owner.clone()),
                );
                if let Some(symbol) = self.symbols.last_mut() {
                    symbol.arity = Some(parameter_types.len());
                }
            }
        }

        if type_kind.is_some()
            || callable
            || matches!(
                node_kind,
                "compound_statement"
                    | "block"
                    | "statements"
                    | "for_statement"
                    | "for_expression"
                    | "lambda_literal"
                    | "lambda_expression"
                    | "catch_clause"
            )
        {
            next_scope = self.scopes.len();
            self.scopes.push(Scope {
                range: node.byte_range(),
                parent: Some(scope),
                owner,
                callable: callable || self.scopes[scope].callable,
            });
        }

        if !callable {
            match node_kind {
                "init_declarator" | "variable_declarator" => {
                    if let Some(name) = node
                        .child_by_field_name("name")
                        .or_else(|| node.child_by_field_name("declarator"))
                        .and_then(declarator_name)
                    {
                        let parent = node.parent().unwrap_or(node);
                        let kind = self.variable_kind(scope);
                        let mut type_name = self.type_for(parent);
                        if type_name.is_empty() {
                            type_name = self.type_for(node);
                        }
                        self.add_symbol(name, kind, scope, type_name, None, true, None);
                    }
                }
                "declaration" | "field_declaration" if self.language == SymbolLanguage::Cpp => {
                    for declarator in (0..node.named_child_count())
                        .filter(|index| {
                            node.field_name_for_named_child(*index as u32) == Some("declarator")
                        })
                        .filter_map(|index| node.named_child(index as u32))
                    {
                        if declarator.kind() != "init_declarator"
                            && let Some(name) = declarator_name(declarator)
                        {
                            let kind = self.variable_kind(scope);
                            let type_name = self.type_for(node);
                            self.add_symbol(name, kind, scope, type_name, None, true, None);
                        }
                    }
                }
                "parameter_declaration"
                | "optional_parameter_declaration"
                | "formal_parameter"
                | "spread_parameter"
                | "parameter"
                | "class_parameter" => {
                    if let Some(name) = node
                        .child_by_field_name("declarator")
                        .and_then(declarator_name)
                        .or_else(|| name_node(node))
                    {
                        let type_name = self.type_for(node);
                        self.add_symbol(name, Kind::Parameter, scope, type_name, None, true, None);
                    }
                }
                "variable_declaration" if self.language == SymbolLanguage::Kotlin => {
                    if let Some(name) = name_node(node) {
                        let kind = self.variable_kind(scope);
                        let mut type_name = self.type_for(node);
                        if type_name.is_empty()
                            && let Some(parent) = node.parent()
                        {
                            type_name = self.type_for(parent);
                        }
                        self.add_symbol(name, kind, scope, type_name, None, true, None);
                    }
                }
                "alias_declaration" | "type_alias" | "typealias_declaration" => {
                    if let Some(name) = name_node(node) {
                        let type_name = self.type_for(node);
                        self.add_symbol(name, Kind::TypeAlias, scope, type_name, None, true, None);
                    }
                }
                _ => {}
            }
        }

        if matches!(
            node_kind,
            "identifier"
                | "simple_identifier"
                | "type_identifier"
                | "field_identifier"
                | "namespace_identifier"
        ) && node.named_child_count() == 0
        {
            let parent = node.parent();
            let receiver = parent.and_then(|parent| {
                let field = parent
                    .child_by_field_name("field")
                    .or_else(|| parent.child_by_field_name("name"));
                if field.is_some_and(|field| field.id() == node.id()) {
                    parent
                        .child_by_field_name("argument")
                        .or_else(|| parent.child_by_field_name("object"))
                        .map(|receiver| normalize(self.source(receiver)))
                } else if matches!(parent.kind(), "qualified_identifier" | "scoped_identifier")
                    && parent
                        .child_by_field_name("name")
                        .is_some_and(|name| name.id() == node.id())
                {
                    parent
                        .child_by_field_name("scope")
                        .map(|receiver| normalize(self.source(receiver)))
                } else if parent.kind() == "navigation_suffix" {
                    parent
                        .parent()
                        .and_then(|navigation| named_children(navigation).next())
                        .map(|receiver| normalize(self.source(receiver)))
                } else {
                    None
                }
            });
            let call = parent
                .filter(|parent| matches!(parent.kind(), "call_expression" | "method_invocation"))
                .or_else(|| {
                    parent.and_then(|parent| parent.parent()).filter(|parent| {
                        matches!(parent.kind(), "call_expression" | "method_invocation")
                    })
                });
            let arity = call
                .and_then(|call| {
                    call.child_by_field_name("arguments").or_else(|| {
                        named_children(call)
                            .find(|child| child.kind() == "call_suffix")
                            .and_then(|suffix| {
                                named_children(suffix)
                                    .find(|child| child.kind() == "value_arguments")
                            })
                    })
                })
                .map(|arguments| arguments.named_child_count());
            self.occurrences.push(Occurrence {
                name: self.source(node).trim_matches('`').to_owned(),
                range: node.byte_range(),
                scope: next_scope,
                receiver,
                arity,
                type_position: node_kind == "type_identifier"
                    || parent.is_some_and(|parent| {
                        matches!(
                            parent.kind(),
                            "user_type" | "generic_type" | "scoped_type_identifier"
                        )
                    }),
            });
        }
        for child in named_children(node) {
            self.visit(child, next_scope, depth + 1);
        }
    }

    fn variable_kind(&self, scope: usize) -> Kind {
        if self.scopes[scope].callable {
            Kind::Variable
        } else if self.type_owners.contains(&self.scopes[scope].owner) {
            Kind::Field
        } else {
            Kind::Variable
        }
    }

    fn type_for(&self, node: Node<'_>) -> String {
        if matches!(node.kind(), "type_alias" | "typealias_declaration") {
            return named_children(node)
                .find(|child| {
                    matches!(
                        child.kind(),
                        "user_type" | "nullable_type" | "function_type"
                    )
                })
                .map(|child| normalize(self.source(child)))
                .unwrap_or_default();
        }
        let type_node = node.child_by_field_name("type").or_else(|| {
            named_children(node).find(|child| {
                matches!(
                    child.kind(),
                    "user_type"
                        | "nullable_type"
                        | "type_identifier"
                        | "integral_type"
                        | "floating_point_type"
                        | "predefined_type"
                )
            })
        });
        if let Some(type_node) = type_node {
            let type_name = normalize(self.source(type_node));
            if type_name != "auto" && type_name != "var" {
                return type_name;
            }
        }
        let value = node.child_by_field_name("value").or_else(|| {
            named_children(node).find(|child| {
                matches!(
                    child.kind(),
                    "object_creation_expression"
                        | "call_expression"
                        | "integer_literal"
                        | "number_literal"
                )
            })
        });
        if let Some(value) = value {
            if value.kind() == "object_creation_expression" {
                return value
                    .child_by_field_name("type")
                    .map(|node| normalize(self.source(node)))
                    .unwrap_or_default();
            }
            if value.kind() == "call_expression" {
                let function = value
                    .child_by_field_name("function")
                    .or_else(|| named_children(value).next());
                if let Some(function) = function {
                    let name = normalize(self.source(function));
                    if name.chars().next().is_some_and(char::is_uppercase) {
                        return name;
                    }
                }
            }
        }
        String::new()
    }

    fn add_symbol(
        &mut self,
        name: Node<'_>,
        kind: Kind,
        scope: usize,
        type_name: String,
        signature: Option<String>,
        definition: bool,
        owner: Option<String>,
    ) {
        let range = name.byte_range();
        if !self.declaration_ranges.insert((range.start, range.end)) {
            return;
        }
        let owner = owner.unwrap_or_else(|| self.scopes[scope].owner.clone());
        if kind.is_type() {
            self.type_owners.insert(qualify(&owner, self.source(name)));
        }
        let mut declaration = name;
        for _ in 0..32 {
            if matches!(
                declaration.kind(),
                "class_specifier"
                    | "struct_specifier"
                    | "union_specifier"
                    | "class_declaration"
                    | "interface_declaration"
                    | "object_declaration"
                    | "record_declaration"
                    | "enum_specifier"
                    | "enum_declaration"
                    | "namespace_definition"
                    | "alias_declaration"
                    | "type_alias"
                    | "typealias_declaration"
                    | "declaration"
                    | "field_declaration"
                    | "method_declaration"
                    | "constructor_declaration"
                    | "function_definition"
                    | "function_declaration"
                    | "property_declaration"
                    | "parameter_declaration"
                    | "formal_parameter"
                    | "parameter"
            ) {
                break;
            }
            let Some(parent) = declaration.parent() else {
                break;
            };
            declaration = parent;
        }
        let declaration_text = self
            .source(declaration)
            .split('{')
            .next()
            .unwrap_or_default();
        let mut ancestor = Some(name);
        let mut anonymous_namespace = false;
        while let Some(node) = ancestor {
            if node.kind() == "namespace_definition" && node.child_by_field_name("name").is_none() {
                anonymous_namespace = true;
                break;
            }
            ancestor = node.parent();
        }
        let mut private = declaration_text
            .split_whitespace()
            .any(|word| word == "private");
        if self.language == SymbolLanguage::Cpp
            && declaration
                .parent()
                .is_some_and(|parent| parent.kind() == "field_declaration_list")
        {
            let mut previous = declaration.prev_named_sibling();
            private = declaration
                .parent()
                .and_then(|parent| parent.parent())
                .is_some_and(|parent| parent.kind() == "class_specifier");
            while let Some(node) = previous {
                if node.kind() == "access_specifier" {
                    private = self.source(node).trim().trim_end_matches(':') != "public";
                    break;
                }
                previous = node.prev_named_sibling();
            }
        }
        self.symbols.push(Symbol {
            name: self.source(name).trim_matches('`').to_owned(),
            kind,
            range,
            scope,
            owner,
            type_name,
            signature,
            arity: None,
            definition,
            local: self.scopes[scope].callable,
            private,
            is_static: declaration_text
                .split_whitespace()
                .any(|word| word == "static"),
            file_local: self.language == SymbolLanguage::Cpp
                && (anonymous_namespace
                    || (!matches!(kind, Kind::Field | Kind::Method)
                        && declaration_text
                            .split_whitespace()
                            .any(|word| word == "static"))),
        });
    }

    fn collect_bases(&mut self, node: Node<'_>, owner: &str, depth: usize) {
        if depth > 16 {
            return;
        }
        if matches!(
            node.kind(),
            "type_identifier" | "user_type" | "scoped_type_identifier" | "qualified_identifier"
        ) {
            self.bases
                .push((owner.to_owned(), normalize(self.source(node))));
            return;
        }
        for child in named_children(node) {
            self.collect_bases(child, owner, depth + 1);
        }
    }

    fn scope_distance(&self, mut scope: usize, ancestor: usize) -> Option<usize> {
        let mut distance = 0;
        loop {
            if scope == ancestor {
                return Some(distance);
            }
            scope = self.scopes.get(scope)?.parent?;
            distance += 1;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SymbolLocation {
    pub document: usize,
    pub symbol: usize,
}

pub struct Index<'a> {
    pub documents: &'a [Arc<Document>],
    symbols_by_name: BTreeMap<&'a str, Vec<SymbolLocation>>,
    dependencies: Vec<Vec<usize>>,
    dependency_sets: Vec<OnceCell<BTreeSet<usize>>>,
}

impl<'a> Index<'a> {
    pub fn new(documents: &'a [Arc<Document>]) -> Self {
        let mut symbols_by_name: BTreeMap<&str, Vec<SymbolLocation>> = BTreeMap::new();
        for (document, file) in documents.iter().enumerate() {
            for (symbol, declaration) in file.symbols.iter().enumerate() {
                symbols_by_name
                    .entry(&declaration.name)
                    .or_default()
                    .push(SymbolLocation { document, symbol });
            }
        }
        let mut dependencies = vec![Vec::new(); documents.len()];
        let paths: BTreeMap<_, _> = documents
            .iter()
            .enumerate()
            .filter_map(|(index, document)| Some((document.path.as_deref()?, index)))
            .collect();
        let mut file_names = BTreeMap::<std::ffi::OsString, Vec<usize>>::new();
        for (path, document) in &paths {
            if let Some(name) = Path::new(path).file_name() {
                file_names
                    .entry(name.to_owned())
                    .or_default()
                    .push(*document);
            }
        }
        for (source, document) in documents.iter().enumerate() {
            let Some(path) = document.path.as_deref() else {
                continue;
            };
            let path = Path::new(path);
            for include in &document.includes {
                let relative_path =
                    normalize_path(&path.parent().unwrap_or_else(|| Path::new("")).join(include));
                if let Some(target) = paths
                    .get(relative_path.to_str().unwrap_or_default())
                    .copied()
                {
                    dependencies[source].push(target);
                    continue;
                }
                let candidates = Path::new(include)
                    .file_name()
                    .and_then(|name| file_names.get(name));
                for target in candidates.into_iter().flatten() {
                    let Some(candidate) = documents[*target].path.as_deref() else {
                        continue;
                    };
                    let candidate = Path::new(candidate);
                    if path.components().next() == candidate.components().next()
                        && candidate.ends_with(include)
                    {
                        dependencies[source].push(*target);
                    }
                }
            }
        }
        Self {
            documents,
            symbols_by_name,
            dependencies,
            dependency_sets: (0..documents.len()).map(|_| OnceCell::new()).collect(),
        }
    }

    pub fn resolve(&self, source: usize, occurrence: &Occurrence) -> Vec<SymbolLocation> {
        let mut candidates = self.candidates(source, occurrence, false);
        candidates.sort_by_key(|(rank, location)| (*rank, *location));
        let Some((best_rank, _)) = candidates.first() else {
            return Vec::new();
        };
        let mut locations: Vec<_> = candidates
            .iter()
            .take_while(|(rank, _)| rank == best_rank)
            .map(|(_, location)| *location)
            .collect();
        if let Some(first) = locations.first().copied() {
            if let Some(declarations) = self.symbols_by_name.get(
                self.documents[first.document].symbols[first.symbol]
                    .name
                    .as_str(),
            ) {
                for declaration in declarations {
                    if !locations.contains(declaration)
                        && locations
                            .iter()
                            .any(|candidate| self.same_symbol(*candidate, *declaration))
                    {
                        locations.push(*declaration);
                    }
                }
            }
        }
        locations
    }

    pub fn complete(&self, source: usize, occurrence: &Occurrence) -> Vec<SymbolLocation> {
        let mut candidates = self.candidates(source, occurrence, true);
        candidates.sort_by_key(|(rank, location)| (*rank, *location));
        let mut seen = BTreeSet::new();
        candidates
            .into_iter()
            .filter_map(|(_, location)| {
                let symbol = &self.documents[location.document].symbols[location.symbol];
                seen.insert((
                    self.name_in_document(source, location).to_owned(),
                    symbol.signature.clone(),
                ))
                .then_some(location)
            })
            .take(80)
            .collect()
    }

    pub fn name_in_document(&self, source: usize, location: SymbolLocation) -> &str {
        let document = &self.documents[location.document];
        let symbol = &document.symbols[location.symbol];
        if !symbol.local
            && let Some(import) = self.documents[source]
                .imports
                .iter()
                .find(|import| !import.wildcard && import.path == qualified(document, symbol))
        {
            return &import.alias;
        }
        &symbol.name
    }

    pub fn same_symbol(&self, left: SymbolLocation, right: SymbolLocation) -> bool {
        if left == right {
            return true;
        }
        let left_document = &self.documents[left.document];
        let right_document = &self.documents[right.document];
        let left_symbol = &left_document.symbols[left.symbol];
        let right_symbol = &right_document.symbols[right.symbol];
        !left_symbol.local
            && !right_symbol.local
            && ((!left_symbol.file_local && !right_symbol.file_local)
                || left.document == right.document)
            && left_document.language == right_document.language
            && left_document.package == right_document.package
            && left_symbol.owner == right_symbol.owner
            && left_symbol.name == right_symbol.name
            && (left_symbol.kind == right_symbol.kind
                || (left_symbol.kind.is_callable() && right_symbol.kind.is_callable()))
            && left_symbol.signature == right_symbol.signature
    }

    fn candidates(
        &self,
        source: usize,
        occurrence: &Occurrence,
        completion: bool,
    ) -> Vec<(usize, SymbolLocation)> {
        let Some(document) = self.documents.get(source) else {
            return Vec::new();
        };
        let owner = &document.scopes[occurrence.scope].owner;
        let receiver = occurrence.receiver.as_ref().and_then(|receiver| {
            if receiver == "this" || receiver == "self" {
                return Some(qualify(&normalize(&document.package), owner));
            }
            if receiver == "super" {
                let bases: Vec<_> = document
                    .bases
                    .iter()
                    .filter(|(derived, _)| derived == owner)
                    .collect();
                if bases.len() != 1 {
                    return None;
                }
                return self.resolve_type(source, occurrence, &bases[0].1);
            }
            let reference = Occurrence {
                name: receiver.clone(),
                receiver: None,
                arity: None,
                type_position: false,
                ..occurrence.clone()
            };
            let resolved = self.resolve(source, &reference);
            if let Some(first) = resolved.first().copied().filter(|first| {
                resolved
                    .iter()
                    .all(|other| self.same_symbol(*first, *other))
            }) {
                let symbol = &self.documents[first.document].symbols[first.symbol];
                if symbol.kind.is_type() {
                    return self.resolve_type(source, occurrence, receiver);
                }
                if symbol.kind == Kind::Namespace {
                    return Some(qualified(&self.documents[first.document], symbol));
                }
                let declaration = Occurrence {
                    scope: symbol.scope,
                    range: symbol.range.clone(),
                    ..reference
                };
                return self.resolve_type(first.document, &declaration, &symbol.type_name);
            }
            receiver.contains("::").then(|| receiver.clone())
        });
        if occurrence.receiver.is_some() && receiver.is_none() {
            return Vec::new();
        }
        let mut matching = Vec::new();
        if completion {
            for (name, symbols) in self.symbols_by_name.range(occurrence.name.as_str()..) {
                if !name.starts_with(&occurrence.name) {
                    break;
                }
                matching.extend(symbols.iter().copied());
            }
        } else if let Some(symbols) = self.symbols_by_name.get(occurrence.name.as_str()) {
            matching.extend(symbols.iter().copied());
        }
        for import in &document.imports {
            if !import.wildcard
                && ((completion && import.alias.starts_with(&occurrence.name))
                    || (!completion && import.alias == occurrence.name))
            {
                if let Some(symbols) = self.symbols_by_name.get(terminal(&import.path)) {
                    matching.extend(symbols.iter().copied());
                }
            }
        }
        matching.sort_unstable();
        matching.dedup();
        let mut candidates = Vec::new();
        for location in matching {
            let document_index = location.document;
            let symbol_index = location.symbol;
            let candidate_document = &self.documents[document_index];
            let symbol = &candidate_document.symbols[symbol_index];
            if candidate_document.language != document.language {
                continue;
            }
            let import_alias = document
                .imports
                .iter()
                .find(|import| {
                    !import.wildcard && import.path == qualified(candidate_document, symbol)
                })
                .map(|import| import.alias.as_str());
            let imported_member = (document.language == SymbolLanguage::Cpp
                || (document.language == SymbolLanguage::Java && symbol.is_static))
                && document.imports.iter().any(|import| {
                    import.path == qualified(candidate_document, symbol)
                        || (import.wildcard
                            && import.path
                                == qualify(&normalize(&candidate_document.package), &symbol.owner))
                });
            let name = import_alias.unwrap_or(&symbol.name);
            if (completion && !name.starts_with(&occurrence.name))
                || (!completion && name != occurrence.name && symbol.name != occurrence.name)
                || (occurrence.type_position && !symbol.kind.is_type())
                || (occurrence.arity.is_some()
                    && symbol.arity.is_some()
                    && occurrence.arity != symbol.arity)
                || (symbol.file_local
                    && document_index != source
                    && !self.dependencies_of(source).contains(&document_index))
                || (symbol.private
                    && (symbol.owner != *owner || document.package != candidate_document.package))
            {
                continue;
            }
            if document.language == SymbolLanguage::Kotlin
                && symbol.kind == Kind::Function
                && candidate_document.package != document.package
                && !document.imports.iter().any(|import| {
                    import.path == qualified(candidate_document, symbol)
                        || (import.wildcard
                            && import.path == normalize(&candidate_document.package))
                })
            {
                continue;
            }
            let rank = if symbol.local {
                if document_index != source || symbol.range.start > occurrence.range.start {
                    continue;
                }
                let Some(distance) = document.scope_distance(occurrence.scope, symbol.scope) else {
                    continue;
                };
                if receiver.is_some() {
                    continue;
                }
                distance
            } else if let Some(receiver) = &receiver {
                if !self.owner_matches(source, receiver, document_index, &symbol.owner) {
                    continue;
                }
                if qualify(&normalize(&candidate_document.package), &symbol.owner) == *receiver {
                    100
                } else {
                    110
                }
            } else if !symbol.owner.is_empty()
                && !imported_member
                && !self.owner_matches(
                    source,
                    &qualify(&normalize(&document.package), owner),
                    document_index,
                    &symbol.owner,
                )
                && !owner.starts_with(&format!("{}::", symbol.owner))
            {
                continue;
            } else if !symbol.owner.is_empty() {
                100
            } else if document_index == source {
                200
            } else if document.language == SymbolLanguage::Cpp
                && self.dependencies_of(source).contains(&document_index)
            {
                250
            } else if document.package == candidate_document.package
                || document.imports.iter().any(|import| {
                    import.path == qualified(candidate_document, symbol)
                        || (import.wildcard
                            && import.path == normalize(&candidate_document.package))
                })
            {
                300
            } else {
                continue;
            };
            candidates.push((
                rank,
                SymbolLocation {
                    document: document_index,
                    symbol: symbol_index,
                },
            ));
        }
        candidates
    }

    fn resolve_type(
        &self,
        mut source: usize,
        occurrence: &Occurrence,
        type_name: &str,
    ) -> Option<String> {
        let mut type_name = normalize(type_name)
            .split('<')
            .next()?
            .trim()
            .trim_end_matches(['*', '&', '?', ' '])
            .to_owned();
        let mut occurrence = occurrence.clone();
        for _ in 0..8 {
            if type_name.is_empty() {
                return None;
            }
            let reference = Occurrence {
                name: terminal(&type_name).to_owned(),
                receiver: None,
                arity: None,
                type_position: true,
                ..occurrence.clone()
            };
            let resolved = if type_name.contains("::") {
                self.symbols_by_name
                    .get(reference.name.as_str())
                    .into_iter()
                    .flatten()
                    .copied()
                    .filter(|location| {
                        let document = &self.documents[location.document];
                        let symbol = &document.symbols[location.symbol];
                        document.language == self.documents[source].language
                            && symbol.kind.is_type()
                            && qualified(document, symbol) == type_name
                    })
                    .collect::<Vec<_>>()
            } else {
                self.resolve(source, &reference)
            };
            let first = resolved.first().copied()?;
            if resolved
                .iter()
                .any(|other| !self.same_symbol(first, *other))
            {
                return None;
            }
            let document = &self.documents[first.document];
            let symbol = &document.symbols[first.symbol];
            if symbol.kind != Kind::TypeAlias {
                return Some(qualified(document, symbol));
            }
            source = first.document;
            type_name = symbol.type_name.clone();
            occurrence.scope = symbol.scope;
            occurrence.range = symbol.range.clone();
        }
        None
    }

    fn dependencies_of(&self, source: usize) -> &BTreeSet<usize> {
        self.dependency_sets[source].get_or_init(|| {
            let mut dependencies = BTreeSet::new();
            let mut pending = self.dependencies[source].clone();
            while let Some(document) = pending.pop() {
                if document != source && dependencies.insert(document) {
                    pending.extend(self.dependencies[document].iter().copied());
                }
            }
            dependencies
        })
    }

    fn owner_matches(&self, source: usize, receiver: &str, candidate: usize, owner: &str) -> bool {
        if owner.is_empty() {
            return false;
        }
        let qualified_owner = qualify(&normalize(&self.documents[candidate].package), owner);
        if qualified_owner == receiver {
            return true;
        }
        let mut pending = vec![receiver.to_owned()];
        let mut seen = BTreeSet::new();
        while let Some(receiver) = pending.pop() {
            if seen.len() >= 32 || !seen.insert(receiver.clone()) {
                continue;
            }
            for (document_index, document) in self
                .documents
                .iter()
                .enumerate()
                .filter(|(_, document)| document.language == self.documents[source].language)
            {
                for (derived, base) in &document.bases {
                    if qualify(&normalize(&document.package), derived) != receiver {
                        continue;
                    }
                    let occurrence = Occurrence {
                        name: base.clone(),
                        range: 0..0,
                        scope: 0,
                        receiver: None,
                        arity: None,
                        type_position: true,
                    };
                    if let Some(base) = self.resolve_type(document_index, &occurrence, base) {
                        if base == qualified_owner {
                            return true;
                        }
                        pending.push(base);
                    }
                }
            }
        }
        false
    }
}

fn named_children(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    (0..node.named_child_count()).filter_map(move |index| node.named_child(index as u32))
}

fn name_node(node: Node<'_>) -> Option<Node<'_>> {
    node.child_by_field_name("name")
        .and_then(declarator_name)
        .or_else(|| {
            named_children(node).find(|child| {
                matches!(
                    child.kind(),
                    "identifier" | "type_identifier" | "simple_identifier"
                )
            })
        })
}

fn declarator_name(node: Node<'_>) -> Option<Node<'_>> {
    let mut node = node;
    for _ in 0..32 {
        if matches!(
            node.kind(),
            "identifier"
                | "type_identifier"
                | "field_identifier"
                | "simple_identifier"
                | "namespace_identifier"
        ) {
            return Some(node);
        }
        node = node
            .child_by_field_name("declarator")
            .or_else(|| node.child_by_field_name("name"))?;
    }
    None
}

fn find_declarator<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    let mut node = node;
    for _ in 0..32 {
        if node.kind() == kind {
            return Some(node);
        }
        node = node.child_by_field_name("declarator")?;
    }
    None
}

fn identifier_character(character: char) -> bool {
    character.is_alphanumeric() || character == '_' || character == '$'
}

fn normalize(text: &str) -> String {
    text.trim()
        .replace('.', "::")
        .trim_end_matches('?')
        .to_owned()
}

fn terminal(text: &str) -> &str {
    text.rsplit("::").next().unwrap_or(text)
}

fn qualify(owner: &str, name: &str) -> String {
    if owner.is_empty() {
        name.to_owned()
    } else {
        format!("{owner}::{name}")
    }
}

fn qualified(document: &Document, symbol: &Symbol) -> String {
    let owner = if document.language == SymbolLanguage::Kotlin && symbol.kind == Kind::Function {
        ""
    } else {
        &symbol.owner
    };
    qualify(&normalize(&document.package), &qualify(owner, &symbol.name))
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    normalized
}
