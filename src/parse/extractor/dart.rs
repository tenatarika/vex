use tree_sitter::Node;

use crate::parse::NodeTextExt;

#[derive(Default, Clone)]
pub(crate) struct ProjectFacts {
    static_methods: std::collections::BTreeSet<String>,
    named_constructors: std::collections::BTreeSet<String>,
}

impl ProjectFacts {
    pub(crate) fn from_source(source: &str) -> anyhow::Result<Self> {
        let tree =
            crate::parse::parser_pool::parse_text(crate::parse::language::Language::Dart, source)?;
        Ok(Self::from_tree(&tree, source.as_bytes()))
    }

    pub(crate) fn from_tree(tree: &tree_sitter::Tree, source: &[u8]) -> Self {
        let mut facts = Self::default();
        let mut pending = vec![tree.root_node()];
        while let Some(node) = pending.pop() {
            match node.kind() {
                "method_declaration" => {
                    if let Some(name) = static_method_identity(node, source) {
                        facts.static_methods.insert(name);
                    }
                }
                kind if is_constructor_signature(kind) => {
                    let identifiers = direct_identifiers(node);
                    if identifiers.len() > 1 {
                        let owner = identifiers[0].node_text(source);
                        let name = identifiers[1].node_text(source);
                        facts.named_constructors.insert(format!("{owner}.{name}"));
                    }
                }
                _ => {}
            }
            let mut cursor = node.walk();
            pending.extend(node.named_children(&mut cursor));
        }
        facts
    }

    pub(crate) fn merge(&mut self, other: Self) {
        self.static_methods.extend(other.static_methods);
        self.named_constructors.extend(other.named_constructors);
    }

    pub(crate) fn from_declarations(
        declarations: &[crate::index::dart_records::DartDeclaration],
    ) -> Self {
        let mut facts = Self::default();
        for declaration in declarations {
            let name = format!("{}.{}", declaration.owner, declaration.member);
            match declaration.kind {
                crate::index::dart_records::DartDeclarationKind::StaticMethod => {
                    facts.static_methods.insert(name);
                }
                crate::index::dart_records::DartDeclarationKind::NamedConstructor => {
                    facts.named_constructors.insert(name);
                }
            }
        }
        facts
    }

    pub(crate) fn declarations(&self) -> Vec<crate::index::dart_records::DartDeclaration> {
        let mut declarations =
            Vec::with_capacity(self.static_methods.len() + self.named_constructors.len());
        for name in &self.static_methods {
            if let Some((owner, member)) = name.rsplit_once('.') {
                declarations.push(crate::index::dart_records::DartDeclaration {
                    owner: owner.to_owned(),
                    member: member.to_owned(),
                    kind: crate::index::dart_records::DartDeclarationKind::StaticMethod,
                });
            }
        }
        for name in &self.named_constructors {
            if let Some((owner, member)) = name.rsplit_once('.') {
                declarations.push(crate::index::dart_records::DartDeclaration {
                    owner: owner.to_owned(),
                    member: member.to_owned(),
                    kind: crate::index::dart_records::DartDeclarationKind::NamedConstructor,
                });
            }
        }
        declarations.sort_by(|a, b| {
            (&a.owner, &a.member, a.kind as u8).cmp(&(&b.owner, &b.member, b.kind as u8))
        });
        declarations
    }

    pub(crate) fn normalize_call(&self, name: &str) -> String {
        if let Some((owner, member)) = name.rsplit_once('.') {
            if member == "new" {
                return owner.to_owned();
            }
            if self.named_constructors.contains(name) {
                return name.to_owned();
            }
            if self.static_methods.contains(name) {
                return member.to_owned();
            }
        }
        name.to_owned()
    }

    pub(crate) fn normalize_ref(&self, name: &str) -> String {
        self.normalize_call(name)
    }

    fn has_known_member(&self, name: &str) -> bool {
        self.named_constructors.contains(name) || self.static_methods.contains(name)
    }
}

fn enclosing_class(mut node: Node<'_>) -> Option<Node<'_>> {
    while let Some(parent) = node.parent() {
        if parent.kind() == "class_definition" {
            return Some(parent);
        }
        node = parent;
    }
    None
}

pub(crate) fn declaration_identity(node: Node<'_>, source: &[u8]) -> Option<String> {
    if node.kind() != "identifier" {
        return None;
    }
    let signature = node.parent()?;
    if !is_constructor_signature(signature.kind()) {
        let method_signature = signature.parent()?;
        if method_signature.kind() != "method_signature" {
            return None;
        }
        let method = method_signature.parent()?;
        if method.kind() != "method_declaration" {
            return None;
        }
        let mut cursor = method_signature.walk();
        if !method_signature
            .children(&mut cursor)
            .any(|child| child.kind() == "static")
        {
            return None;
        }
        enclosing_class(method)?;
        return Some(node.node_text(source).to_owned());
    }
    let identifiers = direct_identifiers(signature);
    if !identifiers.iter().take(2).any(|id| id.id() == node.id()) {
        return None;
    }
    let owner = identifiers[0].node_text(source);
    let name = identifiers.get(1).map_or("new", |n| n.node_text(source));
    if name == "new" {
        return (!owner.is_empty()).then(|| owner.to_owned());
    }
    (!owner.is_empty() && !name.is_empty()).then(|| format!("{owner}.{name}"))
}

pub(crate) fn is_constructor_declaration_identifier(node: Node<'_>) -> bool {
    if node.kind() != "identifier" {
        return false;
    }
    let Some(signature) = node.parent() else {
        return false;
    };
    if !is_constructor_signature(signature.kind()) {
        return false;
    }
    direct_identifiers(signature)
        .iter()
        .take(2)
        .any(|identifier| identifier.id() == node.id())
}

pub(crate) fn call_identity<'tree>(
    node: Node<'tree>,
    source: &[u8],
    facts: &ProjectFacts,
) -> Option<(Node<'tree>, Option<Node<'tree>>, String)> {
    match node.kind() {
        "constructor_invocation" | "new_expression" | "const_object_expression" => {
            let mut owner = None;
            let mut suffix = None;
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                match child.kind() {
                    "type_identifier" | "identifier" if owner.is_none() => owner = Some(child),
                    "type_identifier" | "identifier" if suffix.is_none() => suffix = Some(child),
                    _ => {}
                }
            }
            let owner = owner?;
            let owner_name = owner.node_text(source);
            let name = suffix.map_or("new", |n| n.node_text(source));
            let identity = if name == "new" {
                owner_name.to_owned()
            } else {
                format!("{owner_name}.{name}")
            };
            (!owner_name.is_empty() && !name.is_empty()).then_some((owner, suffix, identity))
        }
        "method_invocation" => {
            let function = node.child_by_field_name("function")?;
            let identifiers = if function.kind() == "identifier" {
                vec![function]
            } else {
                let mut identifiers = Vec::new();
                let mut cursor = function.walk();
                for child in function.named_children(&mut cursor) {
                    if child.kind() == "identifier" {
                        identifiers.push(child);
                    }
                }
                identifiers
            };
            let owner = *identifiers.first()?;
            let owner_name = owner.node_text(source);
            let suffix = identifiers.get(1).copied();
            let member = suffix.map(|n| n.node_text(source));
            let identity = match member {
                Some(member)
                    if owner_name.chars().next()?.is_uppercase()
                        || facts.has_known_member(&format!("{owner_name}.{member}")) =>
                {
                    format!("{owner_name}.{member}")
                }
                None if owner_name.chars().next().is_some_and(char::is_uppercase) => {
                    owner_name.to_owned()
                }
                _ => return None,
            };
            (!owner_name.is_empty() && !identity.is_empty()).then_some((owner, suffix, identity))
        }
        _ => None,
    }
}

fn static_method_identity(method: Node<'_>, source: &[u8]) -> Option<String> {
    let signature = method.child_by_field_name("signature")?;
    let mut signature_cursor = signature.walk();
    let function_signature = signature
        .named_children(&mut signature_cursor)
        .find(|child| child.kind() == "function_signature")?;
    let name = function_signature.child_by_field_name("name")?;
    let mut cursor = signature.walk();
    let is_static = signature
        .children(&mut cursor)
        .any(|child| child.kind() == "static");
    if !is_static {
        return None;
    }
    let owner = enclosing_class(method)?;
    let class_name = owner.child_by_field_name("name")?.node_text(source);
    Some(format!("{class_name}.{}", name.node_text(source)))
}

pub(crate) fn call_identity_for_capture(
    node: Node<'_>,
    source: &[u8],
    facts: &ProjectFacts,
) -> Option<String> {
    let mut current = node;
    while let Some(parent) = current.parent() {
        if is_invocation(parent.kind()) {
            return call_identity(parent, source, facts).map(|(_, _, name)| name);
        }
        if let Some((_, _, name)) = call_identity(parent, source, facts) {
            return Some(name);
        }
        if matches!(
            parent.kind(),
            "function_body" | "block" | "source_file" | "program"
        ) {
            break;
        }
        current = parent;
    }
    None
}

fn is_invocation(kind: &str) -> bool {
    matches!(
        kind,
        "method_invocation"
            | "function_expression_invocation"
            | "constructor_invocation"
            | "new_expression"
            | "const_object_expression"
    )
}

fn direct_identifiers(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| child.kind() == "identifier")
        .collect()
}

fn is_constructor_signature(kind: &str) -> bool {
    matches!(
        kind,
        "constructor_signature"
            | "constant_constructor_signature"
            | "factory_constructor_signature"
            | "redirecting_factory_constructor_signature"
    )
}
