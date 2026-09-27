use std::rc::Rc;

#[derive(Clone, Debug)]
pub enum Ast {
    Num(i64),
    Str(String),
    Bool(bool),
    Ident(String),
    Apply(String, Vec<Ast>),
    At,
    Prime(Box<Ast>),
    Unchanged(Box<Ast>),
    Not(Box<Ast>),
    Neg(Box<Ast>),
    And(Vec<Ast>),
    Or(Vec<Ast>),
    Bin(&'static str, Box<Ast>, Box<Ast>),
    /// `A \X B \X C`, kept n-ary.
    Product(Vec<Ast>),
    /// SUBSET, UNION, DOMAIN, ENABLED
    Prefix(&'static str, Box<Ast>),
    If(Box<Ast>, Box<Ast>, Box<Ast>),
    Case(Vec<(Ast, Ast)>, Option<Box<Ast>>),
    Let(Vec<Rc<Def>>, Box<Ast>),
    Quant(bool, Vec<Bound>, Box<Ast>),
    Choose(Box<Bound>, Box<Ast>),
    SetEnum(Vec<Ast>),
    SetFilter(Box<Bound>, Box<Ast>),
    SetMap(Box<Ast>, Vec<Bound>),
    FuncCons(Vec<Bound>, Box<Ast>),
    FuncSet(Box<Ast>, Box<Ast>),
    Record(Vec<(String, Ast)>),
    RecordSet(Vec<(String, Ast)>),
    Except(Box<Ast>, Vec<(Vec<PathEl>, Ast)>),
    App(Box<Ast>, Vec<Ast>),
    Field(Box<Ast>, String),
    Tuple(Vec<Ast>),
    Lambda(Vec<String>, Box<Ast>),
    /// `[A]_v`
    BoxAction(Box<Ast>, Box<Ast>),
    /// `[]` / `<>` (one operand), `~>` (two), `ENABLED` (one), and fairness
    /// `WF` / `SF` (operands: the subscript, then the action).
    Temporal(&'static str, Vec<Ast>),
}

#[derive(Clone, Debug)]
pub enum PathEl {
    Idx(Vec<Ast>),
    Field(String),
}

#[derive(Clone, Debug)]
pub struct Bound {
    pub names: Vec<String>,
    /// `<<a, b>> \in S`
    pub tuple: bool,
    pub set: Ast,
}

#[derive(Debug)]
pub struct Def {
    pub name: String,
    pub params: Vec<String>,
    pub body: Ast,
}

#[derive(Default, Debug)]
pub struct Module {
    pub name: String,
    pub extends: Vec<String>,
    pub constants: Vec<String>,
    pub variables: Vec<String>,
    pub defs: Vec<Rc<Def>>,
    pub assumes: Vec<Ast>,
}
