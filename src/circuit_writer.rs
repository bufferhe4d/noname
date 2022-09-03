use std::{
    collections::{HashMap, HashSet},
    fmt::{self, Display, Formatter},
    ops::Neg,
    vec,
};

use ark_ff::{One, Zero};
use num_bigint::BigUint;
use num_traits::Num as _;

use crate::{
    asm, boolean,
    constants::{Span, NUM_REGISTERS},
    error::{Error, ErrorKind, Result},
    field::Field,
    imports::{FuncInScope, FuncType, GlobalEnv},
    parser::{
        AttributeKind, Expr, ExprKind, FuncArg, Function, Op2, RootKind, Stmt, StmtKind, TyKind,
    },
    type_checker::TAST,
    witness::{CompiledCircuit, WitnessEnv},
};

//
// Data structures
//

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
/// An internal variable that relates to a specific cell (of the execution trace),
/// or multiple cells (if wired), in the circuit.
///
/// Note: a [CellVar] is potentially not directly added to the rows,
/// for example a private input is converted directly to a (number of) [CellVar],
/// but only added to the rows when it appears in a constraint for the first time.
///
/// As the final step of the compilation,
/// we double check that all cellvars have appeared in the rows at some point.
pub struct CellVar {
    index: usize,
    span: Span,
}

impl CellVar {
    pub fn new(index: usize, span: Span) -> Self {
        Self { index, span }
    }
}

/// A variable's actual value in the witness can be computed in different ways.
pub enum Value {
    /// Either it's a hint and can be computed from the outside.
    Hint(Box<dyn Fn(&CompiledCircuit, &mut WitnessEnv) -> Result<Field>>),

    /// Or it's a constant (for example, I wrote `2` in the code).
    Constant(Field),

    /// Or it's a linear combination of internal circuit variables (+ a constant).
    // TODO: probably values of internal variables should be cached somewhere
    LinearCombination(Vec<(Field, CellVar)>, Field /* cst */),

    Mul(CellVar, CellVar),

    /// A public or private input to the function
    /// There's an index associated to a variable name, as the variable could be composed of several field elements.
    External(String, usize),

    /// A public output.
    /// This is tracked separately as public inputs as it needs to be computed later.
    PublicOutput(Option<CellVar>),
}

impl From<Field> for Value {
    fn from(field: Field) -> Self {
        Self::Constant(field)
    }
}

impl From<usize> for Value {
    fn from(cst: usize) -> Self {
        let cst: u32 = cst
            .try_into()
            .expect("number too large (TODO: better error?)");
        Self::Constant(Field::try_from(cst).unwrap())
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Value::Hint(..) => write!(f, "Hint"),
            Value::Constant(..) => write!(f, "Constant"),
            Value::LinearCombination(..) => write!(f, "LinearCombination"),
            Value::Mul(..) => write!(f, "Mul"),
            Value::External(..) => write!(f, "External"),
            Value::PublicOutput(..) => write!(f, "PublicOutput"),
        }
    }
}

/// The primitive type in a circuit is a field element.
/// But since we want to be able to deal with custom types
/// that are built on top of field elements,
/// we abstract any variable in the circuit as a [Var::CircuitVar]
/// which can contain one or more variables.
#[derive(Debug, Clone)]
pub struct CellVars {
    pub vars: Vec<CellVar>,
    pub span: Span,
}

impl CellVars {
    pub fn new(vars: Vec<CellVar>, span: Span) -> Self {
        Self { vars, span }
    }

    pub fn len(&self) -> usize {
        self.vars.len()
    }

    pub fn is_empty(&self) -> bool {
        self.vars.is_empty()
    }

    pub fn var(&self, i: usize) -> Option<CellVar> {
        self.vars.get(i).cloned()
    }
}

/// Represents an expression or variable in a program
#[derive(Debug, Clone)]
pub enum Var {
    /// Either a constant
    Constant(Constant),
    /// Or a [CellVars]
    CircuitVar(CellVars),
}

#[derive(Debug, Clone)]
pub struct Constant {
    pub value: Field,
    pub span: Span,
}

impl Var {
    pub fn new_circuit_var(vars: Vec<CellVar>, span: Span) -> Self {
        Var::CircuitVar(CellVars::new(vars, span))
    }

    pub fn new_constant(value: Field, span: Span) -> Self {
        Var::Constant(Constant { value, span })
    }

    pub fn constant(&self) -> Option<Field> {
        match self {
            Var::Constant(Constant { value, .. }) => Some(*value),
            _ => None,
        }
    }

    pub fn circuit_var(&self) -> Option<CellVars> {
        match self {
            Var::CircuitVar(circuit_var) => Some(circuit_var.clone()),
            _ => None,
        }
    }

    pub fn is_constant(&self) -> bool {
        matches!(self, Var::Constant(..))
    }

    pub fn span(&self) -> Span {
        match self {
            Var::Constant(Constant { span, .. }) => *span,
            Var::CircuitVar(CellVars { span, .. }) => *span,
        }
    }
}

/// the equivalent of [CellVars] but for witness generation
#[derive(Debug, Clone)]
pub struct CellValues {
    pub values: Vec<Field>,
}

impl CellValues {
    pub fn new(values: Vec<Field>) -> Self {
        Self { values }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum GateKind {
    DoubleGeneric,
    Poseidon,
}

impl From<GateKind> for kimchi::circuits::gate::GateType {
    fn from(gate_kind: GateKind) -> Self {
        use kimchi::circuits::gate::GateType::*;
        match gate_kind {
            GateKind::DoubleGeneric => Generic,
            GateKind::Poseidon => Poseidon,
        }
    }
}

// TODO: this could also contain the span that defined the gate!
#[derive(Debug)]
pub struct Gate {
    /// Type of gate
    pub typ: GateKind,

    /// col -> (row, col)
    // TODO: do we want to do an external wiring instead?
    //    wiring: HashMap<u8, (u64, u8)>,

    /// Coefficients
    pub coeffs: Vec<Field>,

    /// The place in the original source code that created that gate.
    pub span: Span,

    /// A note on why this was added
    pub note: &'static str,
}

impl Gate {
    pub fn to_kimchi_gate(&self, row: usize) -> kimchi::circuits::gate::CircuitGate<Field> {
        kimchi::circuits::gate::CircuitGate {
            typ: self.typ.into(),
            wires: kimchi::circuits::wires::Wire::new(row),
            coeffs: self.coeffs.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cell {
    pub row: usize,
    pub col: usize,
}

impl Display for Cell {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "({},{})", self.row, self.col)
    }
}

#[derive(Debug, Clone)]
pub enum Wiring {
    /// Not yet wired (just indicates the position of the cell itself)
    NotWired(Cell),
    /// The wiring (associated to different spans)
    Wired(Vec<(Cell, Span)>),
}

//
// Circuit Writer (also used by witness generation)
//

#[derive(Default, Debug)]
pub struct CircuitWriter {
    /// The source code that created this circuit.
    /// Useful for debugging and displaying user errors.
    pub source: String,

    /// Once this is set, you can generate a witness (and can't modify the circuit?)
    pub finalized: bool,

    /// This is used to give a distinct number to each variable during circuit generation.
    pub next_variable: usize,

    /// This is how you compute the value of each variable during witness generation.
    /// It is created during circuit generation.
    pub witness_vars: HashMap<CellVar, Value>,

    /// The execution trace table with vars as placeholders.
    /// It is created during circuit generation,
    /// and used by the witness generator.
    pub rows_of_vars: Vec<Vec<Option<CellVar>>>,

    /// The gates created by the circuit generation.
    gates: Vec<Gate>,

    /// The wiring of the circuit.
    /// It is created during circuit generation.
    pub wiring: HashMap<CellVar, Wiring>,

    /// Size of the public input.
    pub public_input_size: usize,

    /// If a public output is set, this will be used to store its [Var::CircuitVar].
    /// The public output generation works as follows:
    /// 1. This cvar is created and inserted in the circuit (gates) during compilation of the public input
    ///    (as the public output is the end of the public input)
    /// 2. When the `return` statement of the circuit is parsed,
    ///    it will set this `public_output` variable again to the correct vars.
    /// 3. During witness generation, the public output computation
    ///    is delayed until the very end.
    pub public_output: Option<CellVars>,

    /// Indexes used by the private inputs
    /// (this is useful to check that they appear in the circuit)
    pub private_input_indices: Vec<usize>,

    /// Used during the witness generation to check
    /// public and private inputs given by the prover.
    pub main_args: (HashMap<String, FuncArg>, Span),
}

impl CircuitWriter {
    pub fn generate_circuit(tast: TAST, code: &str) -> Result<CompiledCircuit> {
        let TAST { ast, global_env } = tast;

        let mut circuit_writer = CircuitWriter {
            source: code.to_string(),
            main_args: global_env.main_args.clone(),
            ..CircuitWriter::default()
        };

        // make sure we can't call that several times
        if circuit_writer.finalized {
            panic!("circuit already finalized (TODO: return a proper error");
        }

        // create the env
        let mut local_env = LocalEnv::default();

        for root in &ast.0 {
            match &root.kind {
                // `use crypto::poseidon;`
                RootKind::Use(_path) => {
                    // we already dealt with that in the type check pass
                }

                // `fn main() { ... }`
                RootKind::Function(function) => {
                    // we only support main for now
                    if !function.is_main() {
                        unimplemented!();
                    }

                    // if there are no arguments, return an error
                    if function.arguments.is_empty() {
                        return Err(Error {
                            kind: ErrorKind::NoArgsInMain,
                            span: function.span,
                        });
                    }

                    // nest scope
                    local_env.nest();

                    // create public and private inputs
                    for FuncArg {
                        attribute,
                        name,
                        typ,
                        ..
                    } in &function.arguments
                    {
                        let len = match &typ.kind {
                            TyKind::Field => 1,
                            TyKind::Array(typ, len) => {
                                if !matches!(**typ, TyKind::Field) {
                                    unimplemented!();
                                }
                                *len as usize
                            }
                            TyKind::Bool => 1,
                            _ => unimplemented!(),
                        };

                        let cvar = if let Some(attr) = attribute {
                            if !matches!(attr.kind, AttributeKind::Pub) {
                                return Err(Error {
                                    kind: ErrorKind::InvalidAttribute(attr.kind),
                                    span: attr.span,
                                });
                            }
                            circuit_writer.add_public_inputs(name.value.clone(), len, name.span)
                        } else {
                            circuit_writer.add_private_inputs(name.value.clone(), len, name.span)
                        };

                        // add argument variable to the ast env
                        local_env.add_var(name.value.clone(), Var::CircuitVar(cvar));
                    }

                    // create public output
                    if let Some(typ) = &function.return_type {
                        if typ.kind != TyKind::Field {
                            unimplemented!();
                        }

                        // create it
                        circuit_writer.public_outputs(1, typ.span);
                    }

                    // compile function
                    circuit_writer.compile_function(&global_env, &mut local_env, function)?;

                    // pop scope
                    local_env.pop();
                }

                // ignore comments
                // TODO: we could actually preserve the comment in the ASM!
                RootKind::Comment(_comment) => (),
            }
        }

        // for sanity check, we make sure that every cellvar created has ended up in a gate
        let mut written_vars = HashSet::new();
        for row in &circuit_writer.rows_of_vars {
            row.iter().flatten().for_each(|cvar| {
                written_vars.insert(cvar.index);
            });
        }

        for var in 0..circuit_writer.next_variable {
            if !written_vars.contains(&var) {
                if circuit_writer.private_input_indices.contains(&var) {
                    return Err(Error {
                        kind: ErrorKind::PrivateInputNotUsed,
                        span: circuit_writer.main_args.1,
                    });
                } else {
                    panic!("there's a bug in the circuit_writer, some cellvar does not end up being a cellvar in the circuit!");
                }
            }
        }

        // we finalized!
        circuit_writer.finalized = true;

        Ok(CompiledCircuit::new(circuit_writer))
    }

    /// Returns the compiled gates of the circuit.
    pub fn compiled_gates(&self) -> &[Gate] {
        if !self.finalized {
            panic!("Circuit not finalized yet!");
        }
        &self.gates
    }

    fn compile_stmt(
        &mut self,
        global_env: &GlobalEnv,
        local_env: &mut LocalEnv,
        stmt: &Stmt,
    ) -> Result<Option<Var>> {
        match &stmt.kind {
            StmtKind::Assign { lhs, rhs, .. } => {
                // compute the rhs
                let var = self
                    .compute_expr(global_env, local_env, rhs)?
                    .ok_or(Error {
                        kind: ErrorKind::CannotComputeExpression,
                        span: stmt.span,
                    })?;

                // store the new variable
                // TODO: do we really need to store that in the scope? That's not an actual var in the scope that's an internal var...
                local_env.add_var(lhs.value.clone(), var);
            }
            StmtKind::For {
                var,
                start,
                end,
                body,
            } => {
                for ii in (*start as usize)..(*end as usize) {
                    local_env.nest();

                    let cst_var = Var::Constant(Constant {
                        value: (ii as u32).into(),
                        span: var.span,
                    });
                    local_env.add_var(var.value.clone(), cst_var);

                    self.compile_block(global_env, local_env, body)?;

                    local_env.pop();
                }
            }
            StmtKind::Expr(expr) => {
                // compute the expression
                let var = self.compute_expr(global_env, local_env, expr)?;

                // make sure it does not return any value.
                assert!(var.is_none());
            }
            StmtKind::Return(res) => {
                let var = self
                    .compute_expr(global_env, local_env, res)?
                    .ok_or(Error {
                        kind: ErrorKind::CannotComputeExpression,
                        span: stmt.span,
                    })?;

                // we already checked in type checking that this is not an early return
                return Ok(Some(var));
            }
            StmtKind::Comment(_) => (),
        }

        Ok(None)
    }

    /// might return something?
    fn compile_block(
        &mut self,
        global_env: &GlobalEnv,
        local_env: &mut LocalEnv,
        stmts: &[Stmt],
    ) -> Result<Option<Var>> {
        local_env.nest();
        for stmt in stmts {
            let res = self.compile_stmt(global_env, local_env, stmt)?;
            if res.is_some() {
                // we already checked for early returns in type checking
                return Ok(res);
            }
        }
        local_env.pop();
        Ok(None)
    }

    /// Compile a function. Used to compile `main()` only for now
    fn compile_function(
        &mut self,
        global_env: &GlobalEnv,
        local_env: &mut LocalEnv,
        function: &Function,
    ) -> Result<()> {
        if !function.is_main() {
            unimplemented!();
        }

        // compile the block
        let returned = self.compile_block(global_env, local_env, &function.body)?;

        // we're expecting something returned?
        match (function.return_type.as_ref(), returned) {
            (None, None) => Ok(()),
            (Some(expected), None) => Err(Error {
                kind: ErrorKind::MissingReturn,
                span: expected.span,
            }),
            (None, Some(returned)) => Err(Error {
                kind: ErrorKind::UnexpectedReturn,
                span: returned.span(),
            }),
            (Some(_expected), Some(returned)) => {
                let var_vars = returned.circuit_var().ok_or_else(|| unimplemented!())?.vars;

                // store the return value in the public input that was created for that ^
                let public_output = self
                    .public_output
                    .as_ref()
                    .expect("bug in the compiler: missing public output");

                for (pub_var, ret_var) in public_output.vars.iter().zip(&var_vars) {
                    // replace the computation of the public output vars with the actual variables being returned here
                    assert!(self
                        .witness_vars
                        .insert(*pub_var, Value::PublicOutput(Some(*ret_var)))
                        .is_some());
                }

                Ok(())
            }
        }
    }

    pub fn asm(&self, debug: bool) -> String {
        asm::generate_asm(&self.source, &self.gates, &self.wiring, debug)
    }

    pub fn new_internal_var(&mut self, val: Value, span: Span) -> CellVar {
        // create new var
        let var = CellVar::new(self.next_variable, span);
        self.next_variable += 1;

        // store it in the circuit_writer
        self.witness_vars.insert(var, val);

        var
    }

    fn compute_expr(
        &mut self,
        global_env: &GlobalEnv,
        local_env: &mut LocalEnv,
        expr: &Expr,
    ) -> Result<Option<Var>> {
        let var: Option<Var> = match &expr.kind {
            ExprKind::FnCall { name, args } => {
                // retrieve the function in the env
                let func: FuncType = if name.len() == 1 {
                    // functions present in the scope
                    let fn_name = &name.path[0].value;
                    match global_env.functions.get(fn_name).ok_or(Error {
                        kind: ErrorKind::UndefinedFunction(fn_name.clone()),
                        span: name.span,
                    })? {
                        FuncInScope::BuiltIn(_, func) => *func,
                        FuncInScope::Library(_, _) => todo!(),
                    }
                } else if name.len() == 2 {
                    // check module present in the scope
                    let module = &name.path[0];
                    let fn_name = &name.path[1];
                    let module = global_env.modules.get(&module.value).ok_or(Error {
                        kind: ErrorKind::UndefinedModule(module.value.clone()),
                        span: module.span,
                    })?;
                    let (_, func) = module.functions.get(&fn_name.value).ok_or(Error {
                        kind: ErrorKind::UndefinedFunction(fn_name.value.clone()),
                        span: fn_name.span,
                    })?;
                    *func
                } else {
                    return Err(Error {
                        kind: ErrorKind::InvalidFnCall("sub-sub modules unsupported"),
                        span: name.span,
                    });
                };

                // compute the arguments
                let mut vars = Vec::with_capacity(args.len());
                for arg in args {
                    let var = self
                        .compute_expr(global_env, local_env, arg)?
                        .ok_or(Error {
                            kind: ErrorKind::CannotComputeExpression,
                            span: arg.span,
                        })?;
                    vars.push(var);
                }

                // run the function
                func(self, &vars, expr.span)
            }
            ExprKind::Assignment { lhs, rhs } => {
                // figure out the local var name of lhs
                let lhs_name = match &lhs.kind {
                    ExprKind::Identifier(n) => n,
                    ExprKind::ArrayAccess(_, _) => todo!(),
                    _ => panic!("type checker error"),
                };

                // figure out the var of what's on the right
                let rhs = self.compute_expr(global_env, local_env, rhs)?.unwrap();

                // replace the left with the right
                local_env.reassign_var(lhs_name, rhs.clone());

                None
            }
            ExprKind::Comparison(_, _, _) => todo!(),
            ExprKind::Op(op, lhs, rhs) => match op {
                Op2::Addition => {
                    let lhs = self.compute_expr(global_env, local_env, lhs)?.unwrap();
                    let rhs = self.compute_expr(global_env, local_env, rhs)?.unwrap();

                    Some(self.add(lhs, rhs, expr.span))
                }
                Op2::Subtraction => todo!(),
                Op2::Multiplication => todo!(),
                Op2::Division => todo!(),
                Op2::Equality => todo!(),
                Op2::BoolAnd => {
                    let lhs = self.compute_expr(global_env, local_env, lhs)?.unwrap();
                    let rhs = self.compute_expr(global_env, local_env, rhs)?.unwrap();

                    Some(boolean::and(self, lhs, rhs, expr.span))
                }
                Op2::BoolOr => todo!(),
                Op2::BoolNot => todo!(),
            },
            ExprKind::Negated(b) => {
                let var = self.compute_expr(global_env, local_env, b)?.unwrap();
                Some(boolean::neg(self, var, expr.span))
            }
            ExprKind::BigInt(b) => {
                let biguint = BigUint::from_str_radix(b, 10).expect("failed to parse number.");
                let f = Field::try_from(biguint).map_err(|_| Error {
                    kind: ErrorKind::CannotConvertToField(b.to_string()),
                    span: expr.span,
                })?;

                Some(Var::new_constant(f, expr.span))
            }
            ExprKind::Bool(b) => {
                let val = if *b { Field::one() } else { Field::zero() };
                Some(Var::new_constant(val, expr.span))
            }
            ExprKind::Identifier(name) => {
                let var = local_env.get_var(name).clone();
                Some(var)
            }
            ExprKind::ArrayAccess(path, expr) => {
                // retrieve the CircuitVar at the path
                let array: CellVars = if path.len() == 1 {
                    // var present in the scope
                    let name = &path.path[0].value;
                    let array_var = local_env.get_var(name);
                    array_var.circuit_var().unwrap()
                } else if path.len() == 2 {
                    // check module present in the scope
                    let module = &path.path[0];
                    let _name = &path.path[1];
                    let _module = global_env.modules.get(&module.value).ok_or(Error {
                        kind: ErrorKind::UndefinedModule(module.value.clone()),
                        span: module.span,
                    })?;
                    unimplemented!()
                } else {
                    return Err(Error {
                        kind: ErrorKind::InvalidPath,
                        span: path.span,
                    });
                };

                // compute the index
                let idx_var = self
                    .compute_expr(global_env, local_env, expr)?
                    .ok_or(Error {
                        kind: ErrorKind::CannotComputeExpression,
                        span: expr.span,
                    })?;

                // the index must be a constant!!
                let idx: Field = idx_var.constant().ok_or(Error {
                    kind: ErrorKind::ExpectedConstant,
                    span: expr.span,
                })?;

                let idx: BigUint = idx.into();
                let idx: usize = idx.try_into().unwrap();

                // index into the CircuitVar
                // (and prevent out of bounds)
                let res = array.var(idx);
                if res.is_none() {
                    return Err(Error {
                        kind: ErrorKind::ArrayIndexOutOfBounds(idx, array.len()),
                        span: expr.span,
                    });
                }

                res.map(|var| Var::new_circuit_var(vec![var], expr.span))
            }
        };

        Ok(var)
    }

    // TODO: dead code?
    pub fn compute_constant(&self, var: CellVar, span: Span) -> Result<Field> {
        match &self.witness_vars.get(&var) {
            Some(Value::Constant(c)) => Ok(*c),
            Some(Value::LinearCombination(lc, cst)) => {
                let mut res = *cst;
                for (coeff, var) in lc {
                    res += self.compute_constant(*var, span)? * *coeff;
                }
                Ok(res)
            }
            Some(Value::Mul(lhs, rhs)) => {
                let lhs = self.compute_constant(*lhs, span)?;
                let rhs = self.compute_constant(*rhs, span)?;
                Ok(lhs * rhs)
            }
            _ => Err(Error {
                kind: ErrorKind::ExpectedConstant,
                span,
            }),
        }
    }

    pub fn num_gates(&self) -> usize {
        self.gates.len()
    }

    fn add(&mut self, lhs: Var, rhs: Var, span: Span) -> Var {
        match (lhs, rhs) {
            (
                Var::Constant(Constant { value: lhs, .. }),
                Var::Constant(Constant { value: rhs, .. }),
            ) => Var::new_constant(lhs + rhs, span),
            (Var::Constant(Constant { value: cst, .. }), Var::CircuitVar(vars))
            | (Var::CircuitVar(vars), Var::Constant(Constant { value: cst, .. })) => {
                if vars.len() != 1 {
                    unimplemented!();
                }
                let var = vars.var(0).unwrap();

                // if the constant is zero, we can ignore this gate
                if cst.is_zero() {
                    return Var::CircuitVar(vars);
                }

                // create a new variable to store the result
                let res = self.new_internal_var(
                    Value::LinearCombination(vec![(Field::one(), var)], cst),
                    span,
                );

                // create a gate to store the result
                // TODO: we should use an add_generic function that takes advantage of the double generic gate
                let zero = Field::zero();
                let one = Field::one();
                self.add_gate(
                    "add a constant with a variable",
                    GateKind::DoubleGeneric,
                    vec![Some(var), None, Some(res)],
                    vec![one, zero, one.neg(), zero, cst],
                    span,
                );

                Var::new_circuit_var(vec![res], span)
            }
            (Var::CircuitVar(lhs), Var::CircuitVar(rhs)) => {
                if lhs.len() != 1 || rhs.len() != 1 {
                    unimplemented!();
                }

                let lhs = lhs.var(0).unwrap();
                let rhs = rhs.var(0).unwrap();

                // create a new variable to store the result
                let res = self.new_internal_var(
                    Value::LinearCombination(
                        vec![(Field::one(), lhs), (Field::one(), rhs)],
                        Field::zero(),
                    ),
                    span,
                );

                // create a gate to store the result
                self.add_gate(
                    "add two variables together",
                    GateKind::DoubleGeneric,
                    vec![Some(lhs), Some(rhs), Some(res)],
                    vec![Field::one(), Field::one(), Field::one().neg()],
                    span,
                );

                Var::new_circuit_var(vec![res], span)
            }
        }
    }

    // TODO: we should cache constants to avoid creating a new variable for each constant
    /// This should be called only when you want to constrain a constant for real.
    /// Gates that handle constants should always make sure to call this function when they want them constrained.
    pub fn add_constant(&mut self, value: Field, span: Span) -> CellVar {
        let var = self.new_internal_var(Value::Constant(value), span);

        let zero = Field::zero();
        self.add_gate(
            "hardcode a constant",
            GateKind::DoubleGeneric,
            vec![Some(var)],
            vec![Field::one(), zero, zero, zero, value.neg()],
            span,
        );

        var
    }

    /// creates a new gate, and the associated row in the witness/execution trace.
    // TODO: add_gate instead of gates?
    pub fn add_gate(
        &mut self,
        note: &'static str,
        typ: GateKind,
        vars: Vec<Option<CellVar>>,
        coeffs: Vec<Field>,
        span: Span,
    ) {
        // sanitize
        assert!(coeffs.len() <= NUM_REGISTERS);
        assert!(vars.len() <= NUM_REGISTERS);

        // construct the execution trace with vars, for the witness generation
        self.rows_of_vars.push(vars.clone());

        // get current row
        // important: do that before adding the gate below
        let row = self.gates.len();

        // add gate
        self.gates.push(Gate {
            typ,
            coeffs,
            span,
            note,
        });

        // wiring (based on vars)
        for (col, var) in vars.iter().enumerate() {
            if let Some(var) = var {
                let curr_cell = Cell { row, col };
                self.wiring
                    .entry(*var)
                    .and_modify(|w| match w {
                        Wiring::NotWired(cell) => {
                            *w = Wiring::Wired(vec![(*cell, var.span), (curr_cell, span)])
                        }
                        Wiring::Wired(ref mut cells) => {
                            cells.push((curr_cell, span));
                        }
                    })
                    .or_insert(Wiring::NotWired(curr_cell));
            }
        }
    }

    pub fn add_public_inputs(&mut self, name: String, num: usize, span: Span) -> CellVars {
        let mut vars = Vec::with_capacity(num);

        for idx in 0..num {
            // create the var
            let var = self.new_internal_var(Value::External(name.clone(), idx), span);
            vars.push(var);

            // create the associated generic gate
            self.add_gate(
                "add public input",
                GateKind::DoubleGeneric,
                vec![Some(var)],
                vec![Field::one()],
                span,
            );
        }

        self.public_input_size += num;

        CellVars::new(vars, span)
    }

    pub fn public_outputs(&mut self, num: usize, span: Span) {
        assert!(self.public_output.is_none());

        let mut vars = Vec::with_capacity(num);
        for _ in 0..num {
            // create the var
            let var = self.new_internal_var(Value::PublicOutput(None), span);
            vars.push(var);

            // create the associated generic gate
            self.add_gate(
                "add public output",
                GateKind::DoubleGeneric,
                vec![Some(var)],
                vec![Field::one()],
                span,
            );
        }
        self.public_input_size += num;

        // store it
        let cvar = CellVars::new(vars, span);
        self.public_output = Some(cvar);
    }

    pub fn add_private_inputs(&mut self, name: String, num: usize, span: Span) -> CellVars {
        let mut vars = Vec::with_capacity(num);

        for idx in 0..num {
            // create the var
            let var = self.new_internal_var(Value::External(name.clone(), idx), span);
            vars.push(var);
            self.private_input_indices.push(var.index);
        }

        CellVars::new(vars, span)
    }
}

//
// Local Environment
//

/// Is used to help functions access their scoped variables.
/// This include inputs and output of the function being processed,
/// as well as local variables.
#[derive(Default, Debug, Clone)]
pub struct LocalEnv {
    /// The current nesting level.
    /// Starting at 0 (top level), and increasing as we go into a block.
    current_scope: usize,

    /// Used by the private and public inputs,
    /// and any other external variables created in the circuit
    /// This needs to be garbage collected when we exit a scope.
    vars: HashMap<String, (usize, Var)>,
}

impl LocalEnv {
    /// Creates a new LocalEnv
    pub fn new() -> Self {
        Self::default()
    }

    /// Enters a scoped block.
    pub fn nest(&mut self) {
        self.current_scope += 1;
    }

    /// Exits a scoped block.
    pub fn pop(&mut self) {
        self.current_scope = self.current_scope.checked_sub(1).expect("scope bug");

        // remove variables as we exit the scope
        // (we don't need to keep them around to detect shadowing,
        // as we already did that in type checker)
        let mut to_delete = vec![];
        for (name, (scope, _)) in self.vars.iter() {
            if *scope > self.current_scope {
                to_delete.push(name.clone());
            }
        }
        for d in to_delete {
            self.vars.remove(&d);
        }
    }

    /// Returns true if a scope is a prefix of our scope.
    pub fn is_in_scope(&self, prefix_scope: usize) -> bool {
        self.current_scope >= prefix_scope
    }

    /// Stores type information about a local variable.
    /// Note that we forbid shadowing at all scopes.
    pub fn add_var(&mut self, name: String, var: Var) {
        if self
            .vars
            .insert(name.clone(), (self.current_scope, var))
            .is_some()
        {
            panic!("type checker error: var {name} already exists");
        }
    }

    /// Retrieves type information on a variable, given a name.
    /// If the variable is not in scope, return false.
    // TODO: return an error no?
    pub fn get_var(&self, ident: &str) -> &Var {
        let (scope, var) = self.vars.get(ident).expect(&format!(
            "type checking bug: local variable {ident} not found"
        ));
        if !self.is_in_scope(*scope) {
            panic!("type checking bug: local variable not in scope");
        }

        var
    }

    pub fn reassign_var(&mut self, ident: &str, var: Var) {
        // get the scope first, we don't want to modify that
        let (scope, _) = self
            .vars
            .get(ident)
            .expect("type checking bug: local variable for reassigning not found");

        if !self.is_in_scope(*scope) {
            panic!("type checking bug: local variable for reassigning not in scope");
        }

        self.vars.insert(ident.to_string(), (*scope, var));
    }
}
