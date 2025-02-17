// This is Burlap's bytecode compiler, it does *not* compile to C or a native instruction set
use std::rc::Rc;
use std::cmp::Ordering;
use std::path::PathBuf;
use std::ptr::null_mut;

use crate::common::IMPOSSIBLE_STATE;
use crate::lexer::TokenType;
use crate::parser::{ASTNode, ASTNode::*, StmtNode, AST, FunctiData, FunctiNode};
use crate::backend::value::Value;
use crate::backend::vm::vm::Opcode;

#[derive(Debug)]
pub struct Program {
    // Opcodes and constants
    pub ops: Vec<u32>,
    pub consts: Vec<Value>,
    // Function locations (name, byte pos, arg num))
    // TODO: Don't use tuple, i32 -> u8
    pub functis: Vec<(String, usize, i32)>,
    // Import dir
    pub path: PathBuf,

    // Side tables
    line_table: Vec<(u32, u32, usize)>,
    file_table: Vec<(u32, u32, String)>,
}

impl Program {
    // Init
    pub fn new() -> Program {
        Program {
            ops: vec![], consts: vec![],
            functis: Vec::new(),
            path: PathBuf::from("."),
            line_table: vec![],
            file_table: vec![],
        }
    }

    fn bin_range<T: Clone>(index: u32, table: &[(u32, u32, T)]) -> Option<T> {
        table.binary_search_by(
            |x| {
                if x.0 > index {
                    Ordering::Greater
                } else if x.1 < index {
                    Ordering::Less
                } else { Ordering::Equal }
            }
        ).map(|x| table[x].2.clone()).ok()
    }

    pub fn get_info(&mut self, index: u32) -> (usize, String) {
        let file = Self::bin_range(index, &self.file_table)
            .unwrap_or("Unknown File".to_string());
        let line = Self::bin_range(index, &self.line_table)
            .unwrap_or(0);
        (line, file)
    }
}

type Reg = u8;
static STACK: Reg = 16;

pub struct Compiler {
    pub program: Program,

    // Variables
    // If VARG and CARG are needed
    needs_args: bool,

    // Side tables
    // Where in the byte code the current file started
    inc_start: u32,
    // Where in the byte code for current line started
    line_start: u32,
    // The current line
    old_line: usize,

    // Loops
    // Break addresses (so the jump can be filled)
    break_addrs: Vec<usize>,
    // Loop top (so continue can be filled)
    loop_top: usize,

    // Registers
    regs: [bool; 17],
    // Limits registers to just the stack
    on_stack_only: bool,

    // The ast
    ast: *mut AST,

    // The current funci
    functi: Option<FunctiData>,
}

impl Compiler {
    pub fn new() -> Compiler {
        Compiler {
            program: Program::new(), old_line: 0,
            regs: [true; 17], needs_args: false,
            break_addrs: vec![], loop_top: 0,
            on_stack_only: false, line_start: 0,
            inc_start: 0, ast: null_mut(), functi: None
        }
    }

    // Instruction wrappers
    #[inline]
    pub fn add_op_args(&mut self, op: Opcode, a: u8, b: u8, c: u8) {
        self.program.ops.push(
            ((op as u32) << 24)
            + ((a as u32) << 16)
            + ((b as u32) << 8)
            + (c as u32)
        );
    }

    #[inline]
    pub fn add_op(&mut self, op: Opcode) {
        self.add_op_args(op, 0, 0, 0);
    }

    fn move_(&mut self, src: Reg, dst: Reg) {
        if dst > 16 {
            panic!("Attempt to mutate a non-mutable register");
        }
        if src == dst {
            return;
        }
        self.copy(src, dst);
        if src == STACK {
            self.add_op(Opcode::POP);
        }
    }

    #[inline]
    fn copy(&mut self, src: Reg, dst: Reg) {
        self.add_op_args(Opcode::CP, src as u8, dst as u8, 0);
    }

    #[inline]
    fn dup(&mut self) {
        self.add_op_args(Opcode::CP, STACK as u8, STACK as u8, 0);
    }

    // Register allocation
    fn alloc_reg(&mut self) -> Reg {
        if self.on_stack_only {
            // Only stack allowed
            return STACK;
        }
        let Some(reg) = self.regs.iter().position(|i| *i) else {
            // No available registers, fallback to stack
            return STACK;
        };
        let reg = reg as u8;
        self.use_reg(reg);
        return reg;
    }

    fn use_reg(&mut self, reg: Reg) {
        self.regs[reg as usize] = false;
    }

    #[inline]
    fn free_reg(&mut self, reg: Reg) {
        if reg == 16 {
            // Why is this commented out?
            //self.add_op(Opcode::POP);
            return;
        } else if reg < 16 {
            self.regs[reg as usize] = true;
        }
    }

    #[inline]
    fn to_mut_reg(&mut self, reg: Reg) -> Reg {
        let new_reg = self.get_mut_reg(reg);
        if reg != new_reg {
            self.move_(reg, new_reg);
        }
        new_reg
    }

    #[inline]
    fn get_mut_reg(&mut self, reg: Reg) -> Reg {
        if !(17 <= reg && reg <= 115) {
            reg
        } else {
            self.alloc_reg()
        }
    }

    #[inline]
    fn get_sole_reg(&mut self, reg: Reg) -> Reg {
        if reg <= 16 {
            reg
        } else {
            self.alloc_reg()
        }
    }

    fn push_to_stack(&mut self, val: Value) {
        // Get the index, or append
        let index = self.program.consts.iter().position(|i| i.clone() == val)
            .unwrap_or_else(|| {
            self.program.consts.push(val);
            self.program.consts.len() - 1
        });
        // Push the instruction
        if index > 2usize.pow(24)-1 {
            panic!("Too many different constants! You have over 16777215 constants!!");
        }
        self.add_op_args(
            Opcode::LDL,
            ((index >> 16) & 255) as u8,
            ((index >> 8) & 255) as u8,
            (index & 255) as u8
        );
    }

    fn push_to(&mut self, val: Value, reg: Option<Reg>) -> Reg {
        // Get the index, or append
        let index = self.program.consts.iter().position(|i| i.clone() == val)
            .unwrap_or_else(|| {
            self.program.consts.push(val);
            self.program.consts.len() - 1
        });
        // Push the instruction
        if index > 2usize.pow(24)-1 {
            panic!("Too many different constants! You have over 16777215 constants!!");
        } else if index > 2usize.pow(16)-1 {
            // The len is too big for two bytes, so use three
            self.add_op_args(
                Opcode::LDL,
                ((index >> 16) & 255) as u8,
                ((index >> 8) & 255) as u8,
                (index & 255) as u8
            );
            if let Some(reg) = reg {
                self.move_(STACK, reg);
                return reg;
            } else {
                return STACK;
            }
        } else {
            // Get a register and push
            let reg = if let Some(r) = reg {
                r
            } else if index < 98 {
                return index as u8 + 17;
            } else {
                self.alloc_reg()
            };
            self.add_op_args(
                Opcode::LD,
                ((index >> 8) & 255) as u8,
                (index & 255) as u8,
                reg as u8
            );
            return reg;
        }
    }

    fn push(&mut self, val: Value) -> Reg {
        self.push_to(val, None)
    }

    fn fill_jmp(&mut self, pos: usize, mut i: usize, reg: Option<Reg>) {
        if i == 0 {
            i = self.program.ops.len() - pos + 1;
        }
         let op = &mut self.program.ops[pos - 1];
        if let Some(x) = reg {
            if i > 2usize.pow(16)-1 {
                panic!("jump offset is over 2 bytes!");
            }
            *op += (x as u8 as u32) << 16;
        } else {
            if i > 2usize.pow(24)-1 {
                panic!("jump offset is over 3 bytes!");
            }
            *op += (i & 255 << 16) as u32;
        }
        *op += (i & 255 << 8) as u32;
        *op += (i & 255) as u32;
    }

    fn get_var_offset(&mut self, var: &String) -> Option<(i32, bool)> {
        let ast = self.get_ast();
        let mut offset = ast.get_var_offset(var.clone(), (&self.functi).as_ref());
        Some(if offset.is_none() && !self.functi.is_none() {
            // Check global too
            offset = offset.or_else(||
                ast.get_var_offset(var.clone(), None)
            );
            (offset? as i32, true)
        } else {
            (offset? as i32, self.functi.is_none())
        })
    }

    fn _var(&mut self, var: &String, reg: Reg, mut op: Opcode) -> Option<()> {
        // Get the offset
        let (offset, global) = self.get_var_offset(var)?;
        if global && (op == Opcode::SV_L || op == Opcode::LV_L) {
            op = if op == Opcode::SV_L { Opcode::SV_G } else { Opcode::LV_G };
        }
        self.add_op_args(
            op,
            ((offset >> 8) & 255) as u8,
            (offset & 255) as u8,
            reg as u8
        );
        Some(())
    }

    fn set_var(&mut self, var: &String, reg: Reg) {
        let op = if self.functi.is_none() { Opcode::SV_G } else { Opcode::SV_L };
        //let var = var.clone().split("::").nth(1).unwrap_or(var).to_string();
        self._var(&var, reg, op).unwrap();
    }

    fn load_var(&mut self, var: &String) -> Reg {
        let reg = self.alloc_reg();
        let op = if self.functi.is_none() { Opcode::LV_G } else { Opcode::LV_L };
        if self._var(var, reg, op).is_none() {
            // It's a function
            self.free_reg(reg);
            let name = var.clone().split("::").nth(1).unwrap_or(var).to_string();
            if name == "__burlap_debug_blackbox" {
                self.push(Value::None)
            } else {
                self.push(Value::Functi(Rc::new(name.clone())))
            }
        } else {
            reg
        }
    }

    #[inline]
    fn get_ast(&mut self) -> &'static mut AST {
        return unsafe { self.ast.as_mut().unwrap() };
    }
}

fn compile_unary(
    compiler: &mut Compiler,
    op: &TokenType, val: &ASTNode
) -> Option<Reg> {
    Some(match op {
        // -/!
        TokenType::Minus => {
            let tmp = compiler.push(Value::Int(0));
            let ret = compile_expr(compiler, val)?;
            let res = compiler.get_sole_reg(ret);
            compiler.add_op_args(Opcode::SUB, tmp as u8, ret as u8, res as u8);
            compiler.free_reg(tmp);
            res
        },
        TokenType::Not => {
            let ret = compile_expr(compiler, val)?;
            let res = compiler.get_sole_reg(ret);
            compiler.add_op_args(Opcode::NOT, ret as u8, res as u8, 0);
            res
        },
        // ++/--
        TokenType::PlusPlus => {
            let ret = compile_expr(compiler, val)?;
            let tmp = compiler.push(Value::Int(1));
            let res = compiler.get_mut_reg(ret);
            compiler.add_op_args(Opcode::ADD, ret as u8, tmp as u8, res as u8);
            if ret == STACK {
                compiler.dup();
            }
            compiler.free_reg(tmp);
            let VarExpr(ref s) = *val else {
                panic!("++ needs a var, how did you do this?");
            };
            compiler.set_var(s, res);
            ret
        },
        TokenType::MinusMinus => {
            let ret = compile_expr(compiler, val)?;
            let tmp = compiler.push(Value::Int(1));
            let res = compiler.get_mut_reg(ret);
            compiler.add_op_args(Opcode::SUB, ret as u8, tmp as u8, res as u8);
            if ret == STACK {
                compiler.dup();
            }
            compiler.free_reg(tmp);
            let VarExpr(ref s) = *val else {
                panic!("-- needs a var, how did you do this?");
            };
            compiler.set_var(s, res);
            res
        },
        _ => panic!("{}", IMPOSSIBLE_STATE),
    })
}


fn compile_set(compiler: &mut Compiler, lvalue: &ASTNode, value: Reg) -> Option<()> {
    // Recursively set
    if let VarExpr(s) = lvalue.clone() {
        compiler.set_var(&s, value);
        compiler.free_reg(value);
        return Some(());
    } else if let IndexExpr(list, index) = lvalue.clone() {
        let ireg = compile_expr(compiler, &index)?;
        let lreg = compile_expr(compiler, &list)?;
        compiler.add_op_args(Opcode::SKY, lreg as u8, ireg as u8, value as u8);
        compiler.free_reg(value);
        compiler.free_reg(ireg);
        // Indexes are attached to something, make sure it reattaches
        return compile_set(compiler, &list, lreg);
    }
    panic!("Cannot compile_set for something other then a variable or index");
}

fn compile_short_binop(
    compiler: &mut Compiler,
    lhs: &ASTNode, op: &TokenType, rhs: &ASTNode,
    clean: bool
) -> Option<Reg> {
    // Compiles short circuiting operators (&& and ||)
    // Uses jump instructions to:
    // Turn `a() && b()` into `r = a(); if r  { r = b() }; r`
    // Turn `a() || b()` into `r = a(); if !r { r = b() }; r`
    let lhs = compile_expr(compiler, lhs)?;
    let dup_tmp = compiler.alloc_reg();
    if lhs == STACK {
        compiler.dup();
    }
    compiler.add_op_args(Opcode::NOT, lhs as u8, dup_tmp as u8, 0);
    if op != &TokenType::Or {
        // Double not is faster then a copy
        compiler.add_op_args(Opcode::NOT, dup_tmp as u8, dup_tmp as u8, 0);
    }
    // Start jump
    compiler.add_op(Opcode::JMPNT);
    let pos = compiler.program.ops.len();
    compiler.free_reg(dup_tmp);
    // It's 'b' (the left)
    if lhs == STACK {
        compiler.add_op(Opcode::POP);
    }
    let rhs = compile_expr(compiler, rhs)?;
    compiler.move_(rhs, lhs);
    compiler.free_reg(rhs);
    // End the jump
    compiler.fill_jmp(pos, 0, Some(dup_tmp));
    if clean {
        if lhs == STACK {
            compiler.add_op(Opcode::POP);
        }
        compiler.free_reg(lhs);
    }
    return Some(lhs);
}

fn compile_binop<'a>(
    compiler: &mut Compiler,
    mut lhs: &'a ASTNode, op: &TokenType, mut rhs: &'a ASTNode,
    clean: bool
) -> Option<Reg> {
    // Short circuiting ops are special
    if op == &TokenType::And || op == &TokenType::Or {
        return compile_short_binop(compiler, lhs, op, rhs, clean);
    }
    // Makes stuff faster
    if op == &TokenType::In {
       (lhs, rhs) = (rhs, lhs);
    }
    // Compile sides
    let lreg = if op != &TokenType::Equals {
        // No need to compile the value if it will just be reassigned
        compile_expr(compiler, lhs)? as u8
    } else {
        // Unused reg so things will break if someone uses it
        47
    };
    let rreg = compile_expr(compiler, rhs)? as u8;
    let resreg = compiler.get_sole_reg(rreg);
    // Compile op
    match op {
        // Simple single instructions
        TokenType::Plus | TokenType::PlusEquals => {
            compiler.add_op_args(Opcode::ADD, lreg, rreg, resreg);
        },
        TokenType::Minus | TokenType::MinusEquals => {
            compiler.add_op_args(Opcode::SUB, lreg, rreg, resreg);
        },
        TokenType::Times | TokenType::TimesEquals => {
            compiler.add_op_args(Opcode::MUL, lreg, rreg, resreg);
        },
        TokenType::Div | TokenType::DivEquals => {
            compiler.add_op_args(Opcode::DIV, lreg, rreg, resreg);
        },
        TokenType::Modulo | TokenType::ModEquals => {
            compiler.add_op_args(Opcode::MOD, lreg, rreg, resreg);
        },
        TokenType::And => {
            compiler.add_op_args(Opcode::AND, lreg, rreg, resreg);
        },
        TokenType::Or => {
            compiler.add_op_args(Opcode::OR, lreg, rreg, resreg);
        },
        TokenType::Xor => {
            compiler.add_op_args(Opcode::XOR, lreg, rreg, resreg);
        },
        TokenType::Gt => {
            compiler.add_op_args(Opcode::GT, lreg, rreg, resreg);
        },
        TokenType::Lt => {
            compiler.add_op_args(Opcode::LT, lreg, rreg, resreg);
        },
        TokenType::EqualsEquals => {
            compiler.add_op_args(Opcode::EQ, lreg, rreg, resreg);
        },
        TokenType::In => {
            compiler.add_op_args(Opcode::IN, lreg, rreg, resreg);
        },
        // Harder ones that don't have a single instruction
        TokenType::NotEquals => {
            compiler.add_op_args(Opcode::EQ, lreg, rreg, resreg);
            compiler.add_op_args(Opcode::NOT, resreg, resreg, 0);
        },
        TokenType::LtEquals => {
            compiler.add_op_args(Opcode::GT, lreg, rreg, resreg);
            compiler.add_op_args(Opcode::NOT, resreg, resreg, 0);
        },
        TokenType::GtEquals => {
            compiler.add_op_args(Opcode::LT, lreg, rreg, resreg);
            compiler.add_op_args(Opcode::NOT, resreg, resreg, 0);
        },
        TokenType::Colon => {
            compiler.add_op_args(Opcode::INX, lreg, rreg, resreg);
        },
        // Handled later
        TokenType::Equals => {},
        _ => panic!("That operator isn't implemented!"),
    };
    // Note to self: rreg does not need to be freed! It's either the same as resreg or not a freeable index
    // Set the variable
    if let TokenType::PlusEquals | TokenType::MinusEquals
        | TokenType::TimesEquals | TokenType::DivEquals
        | TokenType::ModEquals | TokenType::Equals = op.clone()
    {
        let resreg = if *op == TokenType::Equals {
            compiler.free_reg(resreg);
            rreg
        } else {
            compiler.to_mut_reg(resreg)
        };
        compile_set(compiler, lhs, resreg)?;
        return Some(resreg);
    } else if clean {
        // Clean up the stack
        if lreg == STACK as u8 {
            compiler.add_op(Opcode::POP);
        }
        compiler.free_reg(rreg);
        compiler.free_reg(lreg);
        return Some(resreg);
    }
    compiler.free_reg(lreg);
    compiler.free_reg(rreg);
    return Some(resreg);
}

fn compile_call(compiler: &mut Compiler, expr: &ASTNode, args: &Vec<ASTNode>) -> Option<Reg> {
    if let ASTNode::VarExpr(ref n) = *expr {
        let n = n.clone().split("::").nth(1).unwrap().to_string();
        if n == "__burlap_reftype" {
            let Some(VarExpr(name)) = args.get(0) else {
                println!("Compiler Error (internal): __burlap_reftype requires a variable");
                return None;
            };
            let Some((offset, global)) = compiler.get_var_offset(name) else {
                println!("Compiler Error (internal): __burlap_reftype requires a variable");
                return None;
            };
            if global {
                // Global offsets don't change
                return Some(compiler.push(Value::RefType(offset, global)));
            } else {
                // Local offsets do, and need to be figured out at runtime
                let reg = compiler.alloc_reg();
                compiler.add_op_args(
                    Opcode::ALO,
                    ((offset >> 8) & 255) as u8,
                    (offset & 255) as u8,
                    reg as u8
                );
                return Some(reg);
            }
        } else if n == "__burlap_debug_blackbox" {
            return compile_expr(compiler, &args[0]);
        }
    }
    // Push the args onto the stack
    let old_on_stack = compiler.on_stack_only;
    // TODO: Instead of on_stack_only, use a target reg
    compiler.on_stack_only = true;
    for arg in args.iter() {
        let reg = compile_expr(compiler, arg)?;
        if reg != STACK {
            compiler.move_(reg, STACK);
        }
    }
    compiler.on_stack_only = old_on_stack;
    // Get address
    let (address, name) = if let ASTNode::VarExpr(ref n) = *expr {
        // Lookup function address
        let n = n.clone().split("::").nth(1).unwrap().to_string();
        if args.is_empty() && n == "args" {
            // It's `args()`
            compiler.needs_args = true;
            let ret = compiler.alloc_reg();
            // Load saved args
            compiler.add_op_args(Opcode::CARG, ret as u8, 0, 0);
            return Some(ret);
        }
        if let Some(addr) = compiler.program.functis.iter().find_map(
            |i| if i.0 == n && i.2 == args.len() as i32 { Some(i.1) } else { None }
        ) {
            (addr, "".to_string())
        } else {
            // Function isn't static
            (0, "".to_string())
        }
    } else {
        // Function isn't static
        (0, "".to_string())
    };
    // Compile the call
    if address == 0 {
        // Variable call (VCALL)
        let expr = if name.is_empty() || name.contains("::") {
            compile_expr(compiler, expr)?
        } else {
            compiler.push(Value::Functi(Rc::new(name)))
        };
        compiler.add_op_args(Opcode::VCALL, expr as u8, args.len() as u8, 0);
        compiler.free_reg(expr);
        return Some(STACK);
    }

    // Normal static call
    compiler.add_op_args(
        Opcode::CALL,
        ((address >> 16) & 255) as u8,
        ((address >> 8) & 255) as u8,
        (address & 255) as u8
    );
    Some(STACK)
}

fn compile_expr(compiler: &mut Compiler, node: &ASTNode) -> Option<Reg> {
    Some(match node {
        // Values
        VarExpr(val) => {
            compiler.load_var(val)
        },
        StringExpr(val) => {
            compiler.push(Value::Str(Rc::new(val.clone())))
        },
        NumberExpr(val) => {
            compiler.push(Value::Int(*val))
        },
        DecimalExpr(val) => {
            compiler.push(Value::Float(*val))
        },
        BoolExpr(val) => {
            compiler.push(Value::Bool(*val))
        },
        NoneExpr => {
            compiler.push(Value::None)
        },
        ByteExpr(val) => {
            compiler.push(Value::Byte(*val))
        },
        // Binop/unary
        BinopExpr(lhs, op, rhs) => {
            return compile_binop(compiler, lhs, op, rhs, false);
        }
        UnaryExpr(op, val) => {
            return compile_unary(compiler, op, val);
        },
        // Calls
        CallExpr(expr, args) => {
            return compile_call(compiler, expr, args);
        },
        // List
        ListExpr(keys, values, fast) => {
            // Build the list
            let old_on_stack = compiler.on_stack_only;
            compiler.on_stack_only = true;
            for at in 0..values.len() {
                let reg = compile_expr(compiler, &values[at])?;
                if reg != STACK {
                    compiler.move_(reg, STACK);
                    compiler.free_reg(reg);
                }
                if !*fast {
                    compiler.push_to_stack(Value::Str(Rc::new(keys[at].clone())));
                }
            }
            compiler.on_stack_only = old_on_stack;
            // Push
            let len = values.len();
            let reg = compiler.alloc_reg();
            if *fast {
                compiler.add_op_args(
                    Opcode::LFL,
                    reg as u8,
                    ((len >> 8) & 255) as u8,
                    (len & 255) as u8
                );
            } else {
                compiler.add_op_args(
                    Opcode::LL,
                    reg as u8,
                    ((len >> 8) & 255) as u8,
                    (len & 255) as u8
                );
            }
            reg
        },
        // Indexes
        IndexExpr(val, index) => {
            // Push
            let index = compile_expr(compiler, index)?;
            let expr = compile_expr(compiler, val)?;
            compiler.add_op_args(Opcode::INX, expr as u8, index as u8, expr as u8);
            compiler.free_reg(index);
            expr
        },
        // Anonymous functions
        FunctiStmt(node) => {
            compile_functi(compiler, &None, node, false)?;
            compiler.load_var(&node.name)
        },
        // Non-exprs that snuck in
        _ => {
            panic!("Unknown token! {:?}", node);
        }
    })
}

fn _compile_body(
    compiler: &mut Compiler, filename: &Option<String>,
    nodes: &Vec<StmtNode>
) -> Option<()> {
    // Compile all nodes
    for node in nodes {
        compile_stmt(compiler, filename, node, false)?;
    }
    return Some(());
}
fn compile_body(
    compiler: &mut Compiler, filename: &Option<String>, node: &StmtNode
) -> Option<()> {
    let BodyStmt(ref nodes) = node.node else {
        if node.node == Nop {
            return Some(());
        }
        panic!("compile_body got non-body node!");
    };
    _compile_body(compiler, filename, nodes)
}

fn compile_functi(
    compiler: &mut Compiler, filename: &Option<String>, functi: &FunctiNode, _anon: bool
) -> Option<()> {
    let data = compiler.get_ast().get_functi(functi.name.clone()).unwrap();
    // TODO: Assert that old_functi is never Some(...) when anon is false
    let old_functi = compiler.functi.clone();
    compiler.functi = Some(data.clone());
    // Jump around function
    compiler.add_op(Opcode::JMP);
    let pos = compiler.program.ops.len();
    // Declare function
    compiler.program.functis.push((
        functi.name.clone(),
        compiler.program.ops.len(),
        data.arg_num
    ));
    // Arg saving
    let start = compiler.program.ops.len();
    compiler.add_op(Opcode::NOP);
    // Load args from stack
    let arg_num = data.arg_num as usize;
    let lclen = data.locals.len() - arg_num;
    compiler.add_op_args(
        Opcode::PLC,
        ((lclen >> 8) & 255) as u8,
        (lclen & 255) as u8,
        (arg_num & 255) as u8
    );
    // Compile body
    compile_body(compiler, filename, &functi.body)?;
    // Return
    compiler.push_to_stack(Value::None);
    compiler.add_op(Opcode::RET);
    // Save args
    if compiler.needs_args {
        compiler.program.ops[start] = ((Opcode::SARG as u32) << 24)
            + ((arg_num as u32 & 255) << 16);
        compiler.needs_args = false;
    }
    // Fill jump
    compiler.fill_jmp(pos, 0, None);
    compiler.functi = old_functi;
    Some(())
}

fn compile_stmt(
    compiler: &mut Compiler, filename: &Option<String>, node: &StmtNode, dirty: bool
) -> Option<()> {
    if node.line != compiler.old_line {
        compiler.program.line_table.push((
            compiler.line_start, compiler.program.ops.len() as u32, compiler.old_line
        ));
        compiler.line_start = compiler.program.ops.len() as u32;
        compiler.old_line = node.line;
    }
    match &node.node {
        // Statements
        LetStmt(names, vals) => {
            for (name, val) in names.iter().zip(vals.iter()) {
                let vreg = compile_expr(compiler, val)?;
                compiler.set_var(name, vreg);
            }
        },
        IfStmt(cond, body, else_part) => {
            // The condition must be a expr, so no need to match against stmts
            let cond = compile_expr(compiler, cond)?;

            // This is for when boolean not is forgotten
            if body.node == Nop {
                compiler.add_op_args(Opcode::NOT, cond as u8, cond as u8, 0);
                // Push the jump offset (which will be filled later)
                compiler.add_op(Opcode::JMPNT);
                let pos = compiler.program.ops.len();
                // Compile body
                compile_stmt(compiler, filename, else_part, false)?;
                compiler.fill_jmp(pos, 0, Some(cond));
                compiler.free_reg(cond);
                return Some(());
            }

            // Push the jump offset (which will be filled later)
            compiler.add_op(Opcode::JMPNT);
            let pos = compiler.program.ops.len();
            // Compile true part
            compile_body(compiler, filename, body)?;

            // The else
            if else_part.node != Nop {
                // Prep exit offset
                compiler.add_op(Opcode::JMP);
                let exit_pos = compiler.program.ops.len();
                // Fill else jump
                compiler.fill_jmp(pos, 0, Some(cond));
                // Compile else part
                compile_stmt(compiler, filename, else_part, false)?;
                compiler.fill_jmp(exit_pos, 0, None);
            } else {
                // No else
                compiler.fill_jmp(pos, 0, Some(cond));
            }
            compiler.free_reg(cond);
        },
        LoopStmt(body) if body.node == ASTNode::Nop => {
            compiler.add_op(Opcode::NOP);
            compiler.add_op(Opcode::JMPB);
            compiler.fill_jmp(
                compiler.program.ops.len(),
                1,
                None
            );
        },
        LoopStmt(body) => {
            let old_top = compiler.loop_top;
            let last_size = compiler.break_addrs.len();
            compiler.loop_top = compiler.program.ops.len();

            // Body
            compile_body(compiler, filename, body)?;

            // Backwards jump
            compiler.add_op(Opcode::JMPB);
            compiler.fill_jmp(
                compiler.program.ops.len(),
                compiler.program.ops.len() - compiler.loop_top - 1,
                None
            );

            // Fill breaks
            for addr in &compiler.break_addrs.clone()[last_size..] {
                compiler.fill_jmp(*addr, 0, None);
            }
            compiler.break_addrs.truncate(last_size);
            compiler.loop_top = old_top;
        },
        IterLoopStmt(var, iter, body, _already_def) => {
            // Load iter
            let iter = compile_expr(compiler, iter)?;
            compiler.add_op_args(Opcode::ITER, iter as u8, iter as u8, 0);
            let item = compiler.alloc_reg();

            let old_top = compiler.loop_top;
            let last_size = compiler.break_addrs.len();
            compiler.loop_top = compiler.program.ops.len();
            compiler.add_op_args(Opcode::NXT, iter as u8, item as u8, 2);

            // Exit jump
            compiler.add_op(Opcode::JMP);
            let jmp_pos = compiler.program.ops.len();

            // Set loop var
            compiler.set_var(var, item);

            // Body
            compile_body(compiler, filename, body)?;
            // Backwards jump
            compiler.add_op(Opcode::JMPB);
            compiler.fill_jmp(
                compiler.program.ops.len(),
                compiler.program.ops.len() - compiler.loop_top - 1,
                None
            );
            // Fill breaks
            for addr in &compiler.break_addrs.clone()[last_size..] {
                compiler.fill_jmp(*addr, 0, None);
            }
            compiler.break_addrs.truncate(last_size);
            compiler.loop_top = old_top;
            // Clean up the iter
            compiler.fill_jmp(jmp_pos, 0, None);
            compiler.free_reg(iter);
            if iter == STACK {
                compiler.add_op(Opcode::POP);
            }
            compiler.free_reg(item);
        },
        WhileStmt(cond, body) => {
            // Start (so it can loop back)
            let old_top = compiler.loop_top;
            compiler.loop_top = compiler.program.ops.len();
            let last_size = compiler.break_addrs.len();
            // Condition
            let cond = compile_expr(compiler, cond)?;
            // Exit jump
            compiler.add_op(Opcode::JMPNT);
            let exit_jump_pos = compiler.program.ops.len();

            // Compile body
            compile_body(compiler, filename, body)?;

            // Backwards jump
            compiler.add_op(Opcode::JMPB);
            compiler.fill_jmp(
                compiler.program.ops.len(),
                compiler.program.ops.len() - compiler.loop_top - 1,
                None
            );
            // Fill breaks
            for addr in &compiler.break_addrs.clone()[last_size..] {
                compiler.fill_jmp(*addr, 0, None);
            }
            // Exit jump
            compiler.fill_jmp(exit_jump_pos, 0, Some(cond));
            compiler.loop_top = old_top;
            compiler.free_reg(cond);
        },
        BreakStmt => {
            // Filled later
            compiler.add_op(Opcode::JMP);
            compiler.break_addrs.push(compiler.program.ops.len());
        },
        ContinueStmt => {
            compiler.add_op(Opcode::JMPB);
            compiler.fill_jmp(compiler.program.ops.len(), compiler.program.ops.len() - compiler.loop_top - 1, None);
        },
        BodyStmt(nodes) => return _compile_body(compiler, filename, nodes),
        FunctiStmt(node) => {
            return compile_functi(compiler, filename, node, false);
        },
        ReturnStmt(ret) => {
            if let CallExpr(expr, args) = *ret.clone() {
                let functi = compiler.program.functis.last().unwrap().clone();
                let do_tco = if let ASTNode::VarExpr(name) = *expr {
                    name == functi.0 && args.len() == functi.1
                } else { false };
                // Tail call is possible!
                if do_tco {
                    // Push args
                    let old_on_stack = compiler.on_stack_only;
                    compiler.on_stack_only = true;
                    for arg in &args {
                        let reg = compile_expr(compiler, arg)?;
                        if reg != STACK {
                            compiler.move_(reg, STACK);
                            compiler.free_reg(reg);
                        }
                    }
                    compiler.on_stack_only = old_on_stack;
                    // Jump
                    compiler.add_op(Opcode::RCALL);
                    compiler.fill_jmp(
                        compiler.program.ops.len(),
                        compiler.program.ops.len() - functi.2 as usize - 1,
                        None
                    );
                    return Some(());
                }
            }
            // Compile return value
            let old_on_stack = compiler.on_stack_only;
            compiler.on_stack_only = true;
            let reg = compile_expr(compiler, ret)?;
            if reg != STACK {
                compiler.move_(reg, STACK);
                compiler.free_reg(reg);
            }
            compiler.on_stack_only = old_on_stack;
            // Return return value
            compiler.add_op(Opcode::RET);
        },
        ImportStmt => {
            compiler.program.file_table.push((
                compiler.inc_start, compiler.program.ops.len() as u32, filename.clone().unwrap()
            ));
            compiler.inc_start = compiler.program.ops.len() as u32;
        },
        EndImportStmt(file) => {
            compiler.program.file_table.push((
                compiler.inc_start, compiler.program.ops.len() as u32, file.clone()
            ));
            compiler.inc_start = compiler.program.ops.len() as u32;
        },

        Nop => {
            // Nop isn't turned into the NOP instruction because it's useless
        },
        // Expressions
        // Binops don't always return, so let them manage cleaning the stack
        BinopExpr(lhs, op, rhs) => {
            let reg = compile_binop(compiler, lhs, op, rhs, !dirty)?;
            if dirty && reg != STACK {
                // Copy to stack
                compiler.move_(reg, STACK);
                compiler.free_reg(reg);
            }
        },
        _ => {
            let reg = compile_expr(compiler, &node.node)?;
            if !dirty {
                // Remove unused values from the stack
                if reg == STACK {
                    compiler.add_op(Opcode::POP);
                }
            } else if reg != STACK {
                // Move the result to the stack
                compiler.move_(reg, STACK);
            }
            compiler.free_reg(reg);
        }
    };
    return Some(());
}

pub fn compile(
    ast: &mut AST, filename: &Option<String>, compiler: &mut Compiler, repl: bool
) -> bool {
    if ast.nodes.is_empty() {
        return true;
    }
    let gblen = ast.all_vars.len();
    compiler.add_op_args(
        Opcode::PGB,
        ((gblen >> 8) & 255) as u8,
        (gblen & 255) as u8,
        0
    );
    compiler.ast = ast;
    compiler.inc_start = compiler.program.ops.len() as u32;
    // Compile
    for node in &ast.nodes[..ast.nodes.len()-1] {
        if compile_stmt(compiler, filename, node, false).is_none() {
            compiler.ast = null_mut();
            return false;
        }
    }
    // If repl, compile the last value without cleaning up
    // Else just compile normally
    let last = ast.nodes.last().unwrap();
    if compile_stmt(compiler, filename, last, repl).is_none() {
        compiler.ast = null_mut();
        return false;
    }
    // Jumps go onto the next instruction, so a nop is needed at the end
    compiler.add_op(Opcode::NOP);
    // End file
    compiler.program.file_table.push((
        compiler.inc_start, compiler.program.ops.len() as u32, filename.clone().unwrap()
    ));
    compiler.program.line_table.push((
        compiler.line_start, compiler.program.ops.len() as u32, last.line
    ));
    compiler.ast = null_mut();
    return true;
}
