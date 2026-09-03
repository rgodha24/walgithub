//! The document, reduced to the little tree [`super::ops`] dispatches on.
//!
//! There is no GraphQL engine here and there will not be one. Every document
//! the client sends is a literal in its source (`docs/GITHUB_SHAPES.md`,
//! "POST /graphql"), so the facade parses the document, resolves each
//! argument against the JSON `variables` object, and answers on the names.
//! Inline fragments are flattened into their parent's children: the facade
//! knows the concrete type of everything it returns, so `... on Commit` is
//! only a spelling of "these fields too". Named fragment spreads are dropped —
//! no document in the contract has one.

use graphql_parser::query::{Definition, OperationDefinition, Selection};
use serde_json::{Map, Value};

/// The `variables` object of the request body.
pub type Vars = Map<String, Value>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Query,
    Mutation,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Query => "query",
            Kind::Mutation => "mutation",
        }
    }
}

/// One selected field: its name, its arguments with variables substituted,
/// and the fields selected under it.
#[derive(Debug, Default, Clone)]
pub struct Field {
    pub name: String,
    pub args: Map<String, Value>,
    pub children: Vec<Field>,
}

impl Field {
    pub fn child(&self, name: &str) -> Option<&Field> {
        self.children.iter().find(|c| c.name == name)
    }
    pub fn has(&self, name: &str) -> bool {
        self.child(name).is_some()
    }
    pub fn arg(&self, name: &str) -> Option<&Value> {
        self.args.get(name).filter(|v| !v.is_null())
    }
    pub fn str_arg(&self, name: &str) -> Option<&str> {
        self.arg(name).and_then(Value::as_str)
    }
    pub fn usize_arg(&self, name: &str) -> Option<usize> {
        self.arg(name)
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
    }
    /// The `input:` object of a mutation, which is where GitHub puts every
    /// argument of the mutations in the contract.
    pub fn input(&self) -> &Map<String, Value> {
        static EMPTY: std::sync::LazyLock<Map<String, Value>> = std::sync::LazyLock::new(Map::new);
        self.arg("input")
            .and_then(Value::as_object)
            .unwrap_or(&EMPTY)
    }
}

#[derive(Debug)]
pub struct Operation {
    pub kind: Kind,
    pub name: Option<String>,
    pub fields: Vec<Field>,
}

impl Operation {
    /// The operation's name, or its kind when it is anonymous. Every
    /// unimplemented field is reported as `<label>.<field path>`, so a
    /// client's failure names the gap instead of a transport error.
    pub fn label(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| self.kind.as_str().to_string())
    }
}

/// Parse the document and pick its single operation. `operation_name` selects
/// among several; without one the first operation wins, which is what every
/// client in the contract sends.
pub fn parse(query: &str, vars: &Vars, operation_name: Option<&str>) -> Result<Operation, String> {
    let doc = graphql_parser::parse_query::<String>(query)
        .map_err(|e| format!("could not parse the document: {e}"))?;
    let mut first = None;
    for def in doc.definitions {
        let Definition::Operation(op) = def else {
            continue;
        };
        let (kind, name, set) = match op {
            OperationDefinition::Query(q) => (Kind::Query, q.name, q.selection_set),
            OperationDefinition::Mutation(m) => (Kind::Mutation, m.name, m.selection_set),
            OperationDefinition::Subscription(s) => {
                return Err(format!(
                    "subscriptions are not served: {}",
                    s.name.unwrap_or_else(|| "anonymous".to_string())
                ));
            }
            OperationDefinition::SelectionSet(s) => (Kind::Query, None, s),
        };
        let op = Operation {
            kind,
            name,
            fields: fields(&set, vars),
        };
        match operation_name {
            Some(want) if op.name.as_deref() != Some(want) => continue,
            _ => {}
        }
        if first.is_none() || operation_name.is_some() {
            first = Some(op);
            if operation_name.is_some() {
                break;
            }
        }
    }
    first.ok_or_else(|| "the document has no operation".to_string())
}

fn fields(set: &graphql_parser::query::SelectionSet<'_, String>, vars: &Vars) -> Vec<Field> {
    let mut out = Vec::new();
    for item in &set.items {
        match item {
            Selection::Field(f) => out.push(Field {
                name: f.name.clone(),
                args: f
                    .arguments
                    .iter()
                    .map(|(k, v)| (k.clone(), value(v, vars)))
                    .collect(),
                children: fields(&f.selection_set, vars),
            }),
            Selection::InlineFragment(f) => out.extend(fields(&f.selection_set, vars)),
            Selection::FragmentSpread(_) => {}
        }
    }
    out
}

/// A GraphQL literal as JSON, with `$var` replaced by the request's variable
/// (absent variable ⇒ `null`, which is GraphQL's own rule).
fn value(v: &graphql_parser::query::Value<'_, String>, vars: &Vars) -> Value {
    use graphql_parser::query::Value as G;
    match v {
        G::Variable(name) => vars.get(name).cloned().unwrap_or(Value::Null),
        G::Int(n) => n.as_i64().map_or(Value::Null, Value::from),
        G::Float(f) => Value::from(*f),
        G::String(s) => Value::from(s.clone()),
        G::Boolean(b) => Value::from(*b),
        G::Null => Value::Null,
        G::Enum(e) => Value::from(e.clone()),
        G::List(items) => Value::Array(items.iter().map(|i| value(i, vars)).collect()),
        G::Object(o) => Value::Object(
            o.iter()
                .map(|(k, v)| (k.clone(), value(v, vars)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{Kind, parse};
    use serde_json::json;

    fn vars(v: &serde_json::Value) -> super::Vars {
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn variables_are_substituted_into_arguments() {
        let op = parse(
            "query getLatestCommit($owner:String!,$name:String!,$branch:String!){ \
             repository(name:$name, owner:$owner){ ref(qualifiedName:$branch){ target { \
             ... on Commit { history(first:1){ nodes { oid } } } } } } }",
            &vars(&json!({"owner": "acme", "name": "docs", "branch": "main"})),
            None,
        )
        .expect("parse");
        assert_eq!(op.kind, Kind::Query);
        assert_eq!(op.label(), "getLatestCommit");
        let repo = op.fields.first().expect("repository");
        assert_eq!(repo.str_arg("owner"), Some("acme"));
        assert_eq!(repo.str_arg("name"), Some("docs"));
        let r = repo.child("ref").expect("ref");
        assert_eq!(r.str_arg("qualifiedName"), Some("main"));
        // `... on Commit` is flattened into `target`'s children.
        let history = r
            .child("target")
            .and_then(|t| t.child("history"))
            .expect("history");
        assert_eq!(history.usize_arg("first"), Some(1));
    }

    #[test]
    fn a_mutation_input_object_is_read_whole() {
        let op = parse(
            "mutation CreateCommit($input: CreateCommitOnBranchInput!){ \
             createCommitOnBranch(input:$input){ commit { url oid } } }",
            &vars(&json!({"input": {"expectedHeadOid": "abc", "message": {"headline": "hi"}}})),
            None,
        )
        .expect("parse");
        assert_eq!(op.kind, Kind::Mutation);
        let m = op.fields.first().expect("mutation field");
        assert_eq!(m.name, "createCommitOnBranch");
        assert_eq!(m.input()["expectedHeadOid"], "abc");
        assert!(m.child("commit").is_some_and(|c| c.has("oid")));
    }

    #[test]
    fn an_inline_input_literal_works_too() {
        let op = parse(
            "mutation ($id: ID!){ markPullRequestReadyForReview(input:{pullRequestId:$id}){ \
             pullRequest { id } } }",
            &vars(&json!({"id": "PR_1"})),
            None,
        )
        .expect("parse");
        assert_eq!(
            op.fields[0].input().get("pullRequestId"),
            Some(&serde_json::Value::from("PR_1"))
        );
        assert_eq!(op.label(), "mutation");
    }

    #[test]
    fn a_document_that_is_not_graphql_is_an_error() {
        assert!(parse("not graphql at all {{{", &super::Vars::new(), None).is_err());
    }
}
