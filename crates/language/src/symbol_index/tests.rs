use super::*;
use crate::{HighlightSource, Language, LanguageConfig, with_parser};

fn language(kind: SymbolLanguage) -> Language {
    let (name, grammar) = match kind {
        SymbolLanguage::Cpp => ("C++", tree_sitter_cpp::LANGUAGE.into()),
        SymbolLanguage::Java | SymbolLanguage::Kotlin => {
            let name = if kind == SymbolLanguage::Java {
                "java"
            } else {
                "kotlin"
            };
            let directory = std::env::var_os("ZED_SYMBOL_GRAMMAR_DIR")
                .expect("Set ZED_SYMBOL_GRAMMAR_DIR to a directory containing java.wasm and kotlin.wasm from the Zed language extensions");
            let bytes =
                std::fs::read(std::path::PathBuf::from(directory).join(format!("{name}.wasm")))
                    .unwrap();
            let grammar = with_parser(|parser| {
                let mut store = parser.take_wasm_store().unwrap();
                let grammar = store.load_language(name, &bytes).unwrap();
                parser.set_wasm_store(store).unwrap();
                grammar
            });
            (
                if kind == SymbolLanguage::Java {
                    "Java"
                } else {
                    "Kotlin"
                },
                grammar,
            )
        }
    };
    Language::new(
        LanguageConfig {
            name: name.into(),
            ..Default::default()
        },
        Some(grammar),
    )
}

fn document(kind: SymbolLanguage, source: &str) -> Arc<Document> {
    let language = language(kind);
    let tree = HighlightSource::parse(source, language.grammar().unwrap()).unwrap();
    assert!(
        !tree.root_node().has_error(),
        "{}",
        tree.root_node().to_sexp()
    );
    Arc::new(Document::extract(kind, source.into(), tree.root_node()))
}

fn resolved_symbols(documents: &[Arc<Document>], source: usize, offset: usize) -> Vec<&Symbol> {
    let occurrence = documents[source].occurrence_at(offset).unwrap();
    Index::new(documents)
        .resolve(source, &occurrence)
        .iter()
        .map(|location| &documents[location.document].symbols[location.symbol])
        .collect()
}

fn resolved_names(documents: &[Arc<Document>], source: usize, needle: &str) -> Vec<String> {
    let document = &documents[source];
    let offset = document.text.rfind(needle).unwrap();
    let occurrence = document.occurrence_at(offset).unwrap();
    Index::new(documents)
        .resolve(source, &occurrence)
        .iter()
        .map(|location| {
            let symbol = &documents[location.document].symbols[location.symbol];
            format!("{}:{}:{:?}", symbol.owner, symbol.name, symbol.kind)
        })
        .collect()
}

fn completion_names(documents: &[Arc<Document>], source: usize, needle: &str) -> Vec<String> {
    let document = &documents[source];
    let offset = document.text.rfind(needle).unwrap() + needle.len();
    let (_, occurrence) = document.completion_context(offset).unwrap();
    Index::new(documents)
        .complete(source, &occurrence)
        .iter()
        .map(|location| {
            documents[location.document].symbols[location.symbol]
                .name
                .clone()
        })
        .collect()
}

#[test]
fn cpp_resolution_diagnostics_explain_missing_targets() {
    let documents = vec![document(
        SymbolLanguage::Cpp,
        "struct Widget { void reset(); }; struct Other {}; void run(Other other) { other.reset(); unknown.reset(); missing(); }",
    )];
    let index = Index::new(&documents);
    for (needle, reason) in [
        ("other.reset", "receiver_owner_mismatch"),
        ("unknown.reset", "receiver_type_unresolved"),
    ] {
        let offset = documents[0].text.find(needle).unwrap() + needle.find('.').unwrap() + 1;
        let occurrence = documents[0].occurrence_at(offset).unwrap();
        let (locations, diagnostics) = index.resolve_with_diagnostics(0, &occurrence);
        assert!(locations.is_empty());
        assert_eq!(diagnostics.declarations_named, 1);
        assert_eq!(diagnostics.rejected.get(reason), Some(&1));
        assert_eq!(locations, index.resolve(0, &occurrence));
    }
    let occurrence = documents[0]
        .occurrence_at(documents[0].text.find("missing").unwrap())
        .unwrap();
    let (locations, diagnostics) = index.resolve_with_diagnostics(0, &occurrence);
    assert!(locations.is_empty());
    assert_eq!(diagnostics.declarations_named, 0);
    assert!(diagnostics.rejected.is_empty());
}

#[test]
fn cpp_symbols_members_and_shadowing() {
    let documents = vec![document(
        SymbolLanguage::Cpp,
        r#"
        struct Widget { int value; void reset(); };
        int value;
        void run(Widget widget) {
            int value = 1;
            { int value = 2; value++; }
            value++;
            widget.reset();
        }
    "#,
    )];
    assert_eq!(
        resolved_names(&documents, 0, "reset()"),
        ["Widget:reset:Method"]
    );
    let value = documents[0]
        .occurrence_at(documents[0].text.rfind("value++").unwrap())
        .unwrap();
    let index = Index::new(&documents);
    let locations = index.resolve(0, &value);
    assert_eq!(locations.len(), 1);
    let symbol = &documents[0].symbols[locations[0].symbol];
    assert_eq!(
        symbol.range.start,
        documents[0].text.find("value = 1").unwrap()
    );
    assert_eq!(completion_names(&documents, 0, "widget.re"), ["reset"]);
    assert!(
        documents[0]
            .symbols
            .iter()
            .any(|symbol| symbol.name == "widget"
                && symbol.kind == Kind::Parameter
                && symbol.type_name == "Widget")
    );
}

#[test]
fn cpp_cross_file_overloads_and_aliases() {
    let documents = vec![
        document(
            SymbolLanguage::Cpp,
            "struct Widget { void reset(); void reset(int value); }; using Alias = Widget;",
        ),
        document(
            SymbolLanguage::Cpp,
            "void run(Widget widget) { widget.reset(); }",
        ),
    ];
    let targets = resolved_symbols(&documents, 1, documents[1].text.rfind("reset()").unwrap());
    assert_eq!(targets.len(), 1);
    assert_eq!(
        targets[0].range.start,
        documents[0].text.find("reset()").unwrap()
    );
    assert_eq!(
        completion_names(&documents, 1, "widget.re"),
        ["reset", "reset"]
    );
    assert!(
        documents[0]
            .symbols
            .iter()
            .any(|symbol| symbol.name == "Alias"
                && symbol.kind == Kind::TypeAlias
                && symbol.type_name == "Widget")
    );
}

#[test]
fn cpp_references_do_not_merge_shadowed_locals() {
    let documents = vec![document(
        SymbolLanguage::Cpp,
        "void run() { int value = 1; value++; { int value = 2; value++; } value++; }",
    )];
    let index = Index::new(&documents);
    let source = documents[0]
        .occurrence_at(documents[0].text.rfind("value++").unwrap())
        .unwrap();
    let target = index.resolve(0, &source)[0];
    let references: Vec<_> = documents[0]
        .occurrences
        .iter()
        .filter(|occurrence| {
            let resolved = index.resolve(0, occurrence);
            !resolved.is_empty()
                && resolved
                    .iter()
                    .all(|candidate| index.same_symbol(target, *candidate))
        })
        .collect();
    let actual: Vec<_> = references
        .iter()
        .map(|reference| reference.range.start)
        .collect();
    let expected: Vec<_> = documents[0]
        .text
        .match_indices("value++")
        .enumerate()
        .filter_map(|(index, (offset, _))| (index != 1).then_some(offset))
        .collect();
    assert_eq!(actual, expected);
}

#[test]
fn cpp_comments_strings_unicode_and_unknown_receivers() {
    let documents = vec![document(
        SymbolLanguage::Cpp,
        "int café; void run() { café++; /* café */ const char *text = \"café\"; unknown.reset(); }",
    )];
    let document = &documents[0];
    assert_eq!(
        document
            .occurrences
            .iter()
            .filter(|occurrence| occurrence.name == "café")
            .count(),
        1
    );
    assert!(
        document
            .completion_context(document.text.find("/* café").unwrap() + 5)
            .is_none()
    );
    assert!(resolved_names(&documents, 0, "reset()").is_empty());
    assert!(
        document
            .completion_context(document.text.find('é').unwrap() + 1)
            .is_none()
    );
}

#[test]
fn cpp_member_declaration_and_definition_identity() {
    let documents = vec![
        document(SymbolLanguage::Cpp, "struct Widget { void reset(); };"),
        document(SymbolLanguage::Cpp, "void Widget::reset() {}"),
    ];
    let declaration = documents[0]
        .symbols
        .iter()
        .position(|symbol| symbol.name == "reset")
        .unwrap();
    let definition = documents[1]
        .symbols
        .iter()
        .position(|symbol| symbol.name == "reset")
        .unwrap();
    assert!(Index::new(&documents).same_symbol(
        SymbolLocation {
            document: 0,
            symbol: declaration
        },
        SymbolLocation {
            document: 1,
            symbol: definition
        }
    ));
    assert!(!documents[0].symbols[declaration].definition);
    assert!(documents[1].symbols[definition].definition);
}

#[test]
fn cpp_visibility_inference_and_type_alias_receiver() {
    let documents = vec![document(
        SymbolLanguage::Cpp,
        "class Widget { void hidden(); public: void reset(); }; using Alias = Widget; void run(Alias alias) { auto widget = Widget(); alias.reset(); widget.reset(); }",
    )];
    assert_eq!(
        resolved_names(&documents, 0, "reset()"),
        ["Widget:reset:Method"]
    );
    assert_eq!(completion_names(&documents, 0, "alias."), ["reset"]);
    assert_eq!(completion_names(&documents, 0, "widget."), ["reset"]);
}

#[test]
fn cpp_inherited_unqualified_member() {
    let documents = vec![document(
        SymbolLanguage::Cpp,
        "struct Base { void reset(); }; struct Child : Base { void run() { reset(); } };",
    )];
    assert_eq!(
        resolved_names(&documents, 0, "reset()"),
        ["Base:reset:Method"]
    );
}

#[test]
fn cpp_namespace_imports() {
    let documents = vec![document(
        SymbolLanguage::Cpp,
        "namespace tools { void reset(); struct Widget {}; } using namespace tools; void run() { reset(); Widget widget; }",
    )];
    assert_eq!(
        resolved_names(&documents, 0, "reset()"),
        ["tools:reset:Function"]
    );
    assert!(completion_names(&documents, 0, "Wid").contains(&"Widget".to_owned()));
}

#[test]
fn cpp_explicit_type_takes_precedence_over_initializer() {
    let documents = vec![document(
        SymbolLanguage::Cpp,
        "struct Base { void base(); }; struct Child : Base { void child(); }; void run() { Base widget = Child(); widget.base(); }",
    )];
    assert_eq!(completion_names(&documents, 0, "widget."), ["base"]);
}

#[test]
fn cpp_static_header_dependencies_and_anonymous_namespaces() {
    let mut header = document(
        SymbolLanguage::Cpp,
        "#include \"other.hpp\"\nstatic void helper(); static void helper() {}\n",
    );
    Arc::get_mut(&mut header).unwrap().path = Some("project/include/helper.hpp".into());
    let mut other = document(
        SymbolLanguage::Cpp,
        "#include \"helper.hpp\"\nstatic void nested() {}\n",
    );
    Arc::get_mut(&mut other).unwrap().path = Some("project/include/other.hpp".into());
    let mut source = document(
        SymbolLanguage::Cpp,
        "#include \"../include/helper.hpp\"\nvoid run() { helper(); nested(); hidden(); }\n",
    );
    Arc::get_mut(&mut source).unwrap().path = Some("project/src/main.cpp".into());
    let mut unrelated = document(
        SymbolLanguage::Cpp,
        "namespace { void hidden() {} void use_hidden() { hidden(); } }\n",
    );
    Arc::get_mut(&mut unrelated).unwrap().path = Some("project/other.cpp".into());
    let documents = vec![header, other, source, unrelated];
    let index = Index::new(&documents);
    assert!(index.dependencies_of(2).contains(&0));
    assert!(index.dependencies_of(2).contains(&1));
    assert!(!index.dependencies_of(2).contains(&3));
    assert_eq!(
        resolved_names(&documents, 2, "helper()"),
        [":helper:Function", ":helper:Function"]
    );
    assert_eq!(
        resolved_names(&documents, 2, "nested()"),
        [":nested:Function"]
    );
    assert!(
        documents[3]
            .symbols
            .iter()
            .any(|symbol| symbol.name == "hidden" && symbol.file_local)
    );
    assert_eq!(
        resolved_names(&documents, 3, "hidden();"),
        [":hidden:Function"]
    );
    assert!(resolved_names(&documents, 2, "hidden();").is_empty());
    assert!(index.same_symbol(
        SymbolLocation {
            document: 0,
            symbol: 0
        },
        SymbolLocation {
            document: 0,
            symbol: 1
        }
    ));
}

#[test]
#[ignore = "Requires ZED_SYMBOL_GRAMMAR_DIR with the Java extension grammar"]
fn java_packages_imports_inheritance_and_overloads() {
    let documents = vec![
        document(
            SymbolLanguage::Java,
            "package sample; public class Base { public void reset() {} public void reset(int value) {} public int field; }",
        ),
        document(
            SymbolLanguage::Java,
            "package other; import sample.Base; class Child extends Base { void run(Child $widget) { int value = 1; $widget.reset(); value++; } }",
        ),
    ];
    let targets = resolved_symbols(&documents, 1, documents[1].text.rfind("reset()").unwrap());
    assert_eq!(targets.len(), 1);
    assert_eq!(
        targets[0].range.start,
        documents[0].text.find("reset()").unwrap()
    );
    let names = completion_names(&documents, 1, "$widget.re");
    assert_eq!(names, ["reset", "reset"]);
    assert_eq!(
        resolved_names(&documents, 1, "value++"),
        ["Child:value:Variable"]
    );
    assert!(
        documents[1]
            .bases
            .iter()
            .any(|(derived, base)| derived == "Child" && base == "Base")
    );
    assert!(
        documents[0]
            .symbols
            .iter()
            .any(|symbol| symbol.name == "field" && symbol.kind == Kind::Field)
    );
}

#[test]
#[ignore = "Requires ZED_SYMBOL_GRAMMAR_DIR with the Java extension grammar"]
fn java_imports_separate_same_named_types_and_private_members() {
    let documents = vec![
        document(
            SymbolLanguage::Java,
            "package a; public class Widget { public void reset() {} private void hidden() {} }",
        ),
        document(
            SymbolLanguage::Java,
            "package b; public class Widget { public void wrong() {} }",
        ),
        document(
            SymbolLanguage::Java,
            "package caller; import a.Widget; class Main { void run(Widget widget) { widget.reset(); } }",
        ),
    ];
    assert_eq!(completion_names(&documents, 2, "widget."), ["reset"]);
    assert_eq!(
        resolved_names(&documents, 2, "reset()"),
        ["Widget:reset:Method"]
    );
}

#[test]
#[ignore = "Requires ZED_SYMBOL_GRAMMAR_DIR with the Java extension grammar"]
fn java_static_imports() {
    let documents = vec![
        document(
            SymbolLanguage::Java,
            "package tools; public class Helpers { public static void reset() {} }",
        ),
        document(
            SymbolLanguage::Java,
            "package caller; import static tools.Helpers.reset; class Main { void run() { reset(); } }",
        ),
    ];
    assert_eq!(
        resolved_names(&documents, 1, "reset()"),
        ["Helpers:reset:Method"]
    );
}

#[test]
#[ignore = "Requires ZED_SYMBOL_GRAMMAR_DIR with the Kotlin extension grammar"]
fn kotlin_properties_parameters_aliases_and_extensions() {
    let documents = vec![
        document(
            SymbolLanguage::Kotlin,
            "package sample\nclass Widget { val field: Int = 1\n fun reset() {}\n}\nfun Widget.extend() {}\n",
        ),
        document(
            SymbolLanguage::Kotlin,
            "package other\nimport sample.Widget as Alias\nimport sample.extend\nfun run(widget: Alias) { val local: Int = 1\n widget.reset()\n widget.extend()\n local.inc()\n}\n",
        ),
    ];
    assert_eq!(
        resolved_names(&documents, 1, "reset()"),
        ["Widget:reset:Method"]
    );
    assert_eq!(
        resolved_names(&documents, 1, "extend()"),
        ["Widget:extend:Function"],
        "{:#?}",
        documents[0]
    );
    assert!(
        documents[0]
            .symbols
            .iter()
            .any(|symbol| symbol.name == "field" && symbol.kind == Kind::Field)
    );
    assert!(
        documents[1]
            .symbols
            .iter()
            .any(|symbol| symbol.name == "widget"
                && symbol.kind == Kind::Parameter
                && symbol.type_name == "Alias")
    );
    assert!(completion_names(&documents, 1, "widget.re").contains(&"reset".to_owned()));
}

#[test]
#[ignore = "Requires ZED_SYMBOL_GRAMMAR_DIR with the Kotlin extension grammar"]
fn kotlin_type_aliases_preserve_receiver_identity() {
    let documents = vec![
        document(
            SymbolLanguage::Kotlin,
            "package sample\nclass Widget {\n fun reset() {}\n}\ntypealias Alias = Widget\n",
        ),
        document(
            SymbolLanguage::Kotlin,
            "package caller\nimport sample.Alias as Renamed\nfun run(widget: Renamed) {\n widget.reset()\n}\n",
        ),
    ];
    assert_eq!(
        resolved_names(&documents, 1, "reset()"),
        ["Widget:reset:Method"]
    );
}

#[test]
fn cpp_qualified_default_arguments_and_declaration_identity() {
    let document = document(
        SymbolLanguage::Cpp,
        "void reset(); namespace tools { void reset(int value = 0); }
         struct Widget { int value; void run() { int value = 1; ++value; } };
         void choose(int value); void choose(int *value);
         void use_tools() { tools::reset(); double fraction = 1.; }",
    );
    let documents = [document.clone()];
    let offset = |needle| document.text.rfind(needle).unwrap();
    let targets = resolved_symbols(&documents, 0, offset("reset();"));
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].owner, "tools");

    let targets = resolved_symbols(&documents, 0, offset("value = 1"));
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].range.start, offset("value = 1"));
    assert!(targets[0].local);

    for (needle, signature) in [("choose(int value)", "int"), ("choose(int *value)", "int*")] {
        let targets = resolved_symbols(&documents, 0, offset(needle));
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].signature.as_deref(), Some(signature));
    }

    assert!(document.completion_context(offset("1.;") + 2).is_none());
}

#[test]
fn cpp_function_identity_variadics_and_global_completion() {
    let document = document(
        SymbolLanguage::Cpp,
        r#"void match(const int);
         void match(int value) {}
         void pointee(int const *arg);
         void pointee(const int *value) {}
         void pointee(int *value);
         void pointer(int *const);
         void pointer(int *value) {}
         void zero(void);
         void zero() {}
         void log(const char *, ...);
         struct Widget { void run(int); void run(const int) const; };
         int global();
         namespace other { int global(); int run() {
             log("%d", 1);
             zero();
             (void)sizeof(::global());
             (void)(true ? 0 : ::global());
             return ::global();
         } }"#,
    );
    let index = Index::new(std::slice::from_ref(&document));
    for (declaration, target_count) in [
        ("match(const", 2),
        ("pointee(int const", 2),
        ("pointer(int *const", 2),
        ("zero(void", 2),
        (r#"log("%d"#, 1),
        ("zero();", 2),
        ("run(int)", 1),
        ("run(const int)", 1),
    ] {
        let offset = document.text.find(declaration).unwrap();
        let occurrence = document.occurrence_at(offset).unwrap();
        assert_eq!(
            index.resolve(0, &occurrence).len(),
            target_count,
            "{declaration}"
        );
    }
    let qualified = resolved_symbols(
        std::slice::from_ref(&document),
        0,
        document.text.find("run(const int)").unwrap(),
    );
    assert_eq!(qualified[0].signature.as_deref(), Some("int"));
    assert_eq!(qualified[0].method_qualifiers, " const");
    for (start, _) in document.text.match_indices("::global()") {
        let offset = start + "::glo".len();
        let (_, occurrence) = document.completion_context(offset).unwrap();
        let targets = index.complete(0, &occurrence);
        assert_eq!(targets.len(), 1);
        assert!(document.symbols[targets[0].symbol].owner.is_empty());
    }
}

#[test]
#[ignore = "Requires ZED_SYMBOL_GRAMMAR_DIR with the Kotlin extension grammar"]
fn kotlin_top_level_private_functions_are_file_local() {
    let documents = [
        document(
            SymbolLanguage::Kotlin,
            "package sample
private fun helper() {}
fun first() { helper() }
",
        ),
        document(
            SymbolLanguage::Kotlin,
            "package sample
private fun helper() {}
fun second() { helper() }
",
        ),
        document(
            SymbolLanguage::Kotlin,
            "package sample
fun third() { helper() }
",
        ),
    ];
    let index = Index::new(&documents);
    for (source, document) in documents.iter().enumerate() {
        let offset = document.text.rfind("helper()").unwrap();
        let targets = index.resolve(source, &document.occurrence_at(offset).unwrap());
        if source < 2 {
            assert_eq!(targets.len(), 1);
            assert_eq!(targets[0].document, source);
        } else {
            assert!(targets.is_empty());
        }
    }
}
