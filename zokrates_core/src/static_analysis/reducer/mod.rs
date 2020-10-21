// The reducer reduces the program to a single function which is:
// - in SSA form
// - free of function calls (except for low level calls) thanks to inlining
// - free of for-loops thanks to unrolling

// The process happens in two steps
// 1. Shallow SSA for the `main` function
// We turn the `main` function into SSA form, but ignoring function calls and for loops
// 2. Unroll and inline
// We go through the shallow-SSA program and
// - unroll loops
// - inline function calls. This includes applying shallow-ssa on the target function

mod inline;
mod shallow_ssa;
mod unroll;

use self::inline::inline_call;
use std::collections::HashMap;
use typed_absy::Folder;
use typed_absy::{
    CoreIdentifier, Signature, TypedExpressionList, TypedFunction, TypedFunctionSymbol,
    TypedModule, TypedModules, TypedProgram, TypedStatement,
};
use zokrates_field::Field;

use self::shallow_ssa::ShallowTransformer;

// An SSA version map, giving access to the latest version number for each identifier
pub type Versions<'ast> = HashMap<CoreIdentifier<'ast>, usize>;

// A container to represent whether more treatment must be applied to the function
#[derive(Debug, PartialEq)]
pub enum Output<'ast, T> {
    Complete(TypedFunction<'ast, T>),
    Incomplete(TypedFunction<'ast, T>, Vec<Versions<'ast>>, Versions<'ast>),
}

#[derive(Debug)]
pub enum Error {
    Generic,
}

struct Reducer<'ast, T> {
    generics: Vec<u32>,
    modules: TypedModules<'ast, T>,
    error: Option<Error>,
}

pub fn reduce_program<T: Field>(p: TypedProgram<T>) -> Result<TypedProgram<T>, Error> {
    let main_module = p.modules.get(&p.main).unwrap().clone();

    let (main_key, main_function) = main_module
        .functions
        .iter()
        .find(|(k, _)| k.id == "main")
        .unwrap()
        .clone();

    let main_function = match main_function {
        TypedFunctionSymbol::Here(f) => f.clone(),
        _ => unreachable!(),
    };

    assert_eq!(main_function.generics.len(), 0);

    let mut reducer = Reducer {
        generics: vec![],
        modules: p.modules.clone(),
        error: None,
    };

    let main_function = reducer.fold_function(main_function);

    match reducer.error {
        Some(e) => Err(e),
        None => Ok(TypedProgram {
            main: p.main.clone(),
            modules: vec![(
                p.main,
                TypedModule {
                    functions: vec![(main_key.clone(), TypedFunctionSymbol::Here(main_function))]
                        .into_iter()
                        .collect(),
                },
            )]
            .into_iter()
            .collect(),
        }),
    }
}

impl<'ast, T: Field> Folder<'ast, T> for Reducer<'ast, T> {
    fn fold_function(&mut self, f: TypedFunction<'ast, T>) -> TypedFunction<'ast, T> {
        match ShallowTransformer::transform(f, self.generics.clone()) {
            Output::Complete(f) => f,
            Output::Incomplete(new_f, new_for_loop_versions, new_versions) => {
                let mut versions = new_versions;
                let mut for_loop_versions = new_for_loop_versions;

                let mut f = new_f;

                let statements = loop {
                    match reduce_statements(
                        f.statements,
                        for_loop_versions,
                        versions,
                        &self.modules,
                    ) {
                        Ok(statements) => {
                            break statements;
                        }
                        Err((new_statements, new_for_loop_versions, new_versions)) => {
                            let new_f = TypedFunction {
                                statements: new_statements,
                                ..f
                            };

                            f = propagate(new_f);
                            versions = new_versions;
                            for_loop_versions = new_for_loop_versions;
                        }
                    }
                };

                TypedFunction { statements, ..f }
            }
        }
    }
}

fn reduce_statements<'ast, T: Field>(
    statements: Vec<TypedStatement<'ast, T>>,
    for_loop_versions: Vec<Versions<'ast>>,
    versions: Versions<'ast>,
    modules: &TypedModules<'ast, T>,
) -> Result<
    Vec<TypedStatement<'ast, T>>,
    (
        Vec<TypedStatement<'ast, T>>,
        Vec<Versions<'ast>>,
        Versions<'ast>,
    ),
> {
    let mut versions = versions;
    let mut for_loop_versions = for_loop_versions;
    let statements = statements
        .into_iter()
        .map(|s| reduce_statement(s, &mut for_loop_versions, &mut versions, modules));

    let statements = statements
        .into_iter()
        .fold(Ok(vec![]), |state, e| match (state, e) {
            (Ok(mut v), Ok(stats)) => {
                v.extend(stats);
                Ok(v)
            }
            (Ok(mut v), Err(stats)) => {
                v.extend(stats);
                Err(v)
            }
            (Err(mut v), Ok(stats)) => {
                v.extend(stats);
                Err(v)
            }
            (Err(mut v), Err(stats)) => {
                v.extend(stats);
                Err(v)
            }
        });

    statements.map_err(|statements| (statements, for_loop_versions, versions))
}

fn reduce_statement<'ast, T: Field>(
    statement: TypedStatement<'ast, T>,
    _: &mut Vec<Versions>,
    _: &mut Versions<'ast>,
    modules: &TypedModules<'ast, T>,
) -> Result<Vec<TypedStatement<'ast, T>>, Vec<TypedStatement<'ast, T>>> {
    match statement {
        TypedStatement::MultipleDefinition(
            v,
            TypedExpressionList::FunctionCall(key, arguments, types),
        ) => inline_call(v, "main".into(), key, arguments, modules),
        TypedStatement::For(..) => unimplemented!(),
        s => Ok(vec![s]),
    }
}

fn propagate<'ast, T>(f: TypedFunction<'ast, T>) -> TypedFunction<'ast, T> {
    f
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::typed_absy::{
        ArrayExpressionInner, ConcreteFunctionKey, ConcreteSignature, ConcreteType,
        ConcreteVariable, DeclarationFunctionKey, DeclarationType, DeclarationVariable,
        FieldElementExpression, FunctionKey, Identifier, Type, TypedExpression,
        TypedExpressionList, UBitwidth, UExpressionInner, Variable,
    };
    use typed_absy::types::Constant;
    use typed_absy::types::DeclarationSignature;
    use zokrates_field::Bn128Field;

    #[test]
    fn no_generics() {
        // def foo(field a) -> field:
        //      return a
        // def main(field a) -> field:
        //      u32 n = 42
        //      n = n
        //      a = a
        //      a = foo(a)
        //      n = n
        //      return a

        // expected:
        // def main(field a_0) -> field:
        //      u32 n_0 = 42
        //      n_1 = n_0
        //      a_1 = a_0
        //      # PUSH CALL to foo with a_0 := a_1
        //      # POP CALL with a_2 := a_0
        //      n_2 = n_1
        //      return a_2

        let foo: TypedFunction<Bn128Field> = TypedFunction {
            generics: vec![],
            arguments: vec![DeclarationVariable::field_element("a").into()],
            statements: vec![TypedStatement::Return(vec![
                FieldElementExpression::Identifier("a".into()).into(),
            ])],
            signature: DeclarationSignature::new()
                .inputs(vec![DeclarationType::FieldElement])
                .outputs(vec![DeclarationType::FieldElement]),
        };

        let main: TypedFunction<Bn128Field> = TypedFunction {
            generics: vec![],
            arguments: vec![DeclarationVariable::field_element("a").into()],
            statements: vec![
                TypedStatement::Definition(
                    Variable::uint("n", UBitwidth::B32).into(),
                    TypedExpression::Uint(42u32.into()),
                ),
                TypedStatement::Definition(
                    Variable::uint("n", UBitwidth::B32).into(),
                    UExpressionInner::Identifier("n".into())
                        .annotate(UBitwidth::B32)
                        .into(),
                ),
                TypedStatement::Definition(
                    Variable::field_element("a").into(),
                    FieldElementExpression::Identifier("a".into()).into(),
                ),
                TypedStatement::MultipleDefinition(
                    vec![Variable::field_element("a").into()],
                    TypedExpressionList::FunctionCall(
                        DeclarationFunctionKey::with_id("foo").signature(
                            DeclarationSignature::new()
                                .inputs(vec![DeclarationType::FieldElement])
                                .outputs(vec![DeclarationType::FieldElement]),
                        ),
                        vec![FieldElementExpression::Identifier("a".into()).into()],
                        vec![Type::FieldElement],
                    ),
                ),
                TypedStatement::Definition(
                    Variable::uint("n", UBitwidth::B32).into(),
                    UExpressionInner::Identifier("n".into())
                        .annotate(UBitwidth::B32)
                        .into(),
                ),
                TypedStatement::Return(vec![FieldElementExpression::Identifier("a".into()).into()]),
            ],
            signature: DeclarationSignature::new()
                .inputs(vec![DeclarationType::FieldElement])
                .outputs(vec![DeclarationType::FieldElement]),
        };

        let p = TypedProgram {
            main: "main".into(),
            modules: vec![(
                "main".into(),
                TypedModule {
                    functions: vec![
                        (
                            DeclarationFunctionKey::with_id("foo").signature(
                                DeclarationSignature::new()
                                    .inputs(vec![DeclarationType::FieldElement])
                                    .outputs(vec![DeclarationType::FieldElement]),
                            ),
                            TypedFunctionSymbol::Here(foo),
                        ),
                        (
                            DeclarationFunctionKey::with_id("main").signature(
                                DeclarationSignature::new()
                                    .inputs(vec![DeclarationType::FieldElement])
                                    .outputs(vec![DeclarationType::FieldElement]),
                            ),
                            TypedFunctionSymbol::Here(main),
                        ),
                    ]
                    .into_iter()
                    .collect(),
                },
            )]
            .into_iter()
            .collect(),
        };

        let reduced = reduce_program(p);

        let expected_main = TypedFunction {
            generics: vec![],
            arguments: vec![DeclarationVariable::field_element("a").into()],
            statements: vec![
                TypedStatement::Definition(
                    Variable::uint("n", UBitwidth::B32).into(),
                    TypedExpression::Uint(42u32.into()),
                ),
                TypedStatement::Definition(
                    Variable::uint(Identifier::from("n").version(1), UBitwidth::B32).into(),
                    UExpressionInner::Identifier("n".into())
                        .annotate(UBitwidth::B32)
                        .into(),
                ),
                TypedStatement::Definition(
                    Variable::field_element(Identifier::from("a").version(1)).into(),
                    FieldElementExpression::Identifier("a".into()).into(),
                ),
                TypedStatement::PushCallLog(
                    "main".into(),
                    DeclarationFunctionKey::with_id("foo").signature(
                        DeclarationSignature::new()
                            .inputs(vec![DeclarationType::FieldElement])
                            .outputs(vec![DeclarationType::FieldElement]),
                    ),
                    vec![],
                    vec![(
                        ConcreteVariable::with_id_and_type("a", ConcreteType::FieldElement),
                        FieldElementExpression::Identifier(Identifier::from("a").version(1)).into(),
                    )],
                ),
                TypedStatement::PopCallLog(vec![(
                    ConcreteVariable::with_id_and_type(
                        Identifier::from("a").version(2),
                        ConcreteType::FieldElement,
                    ),
                    FieldElementExpression::Identifier("a".into()).into(),
                )]),
                TypedStatement::Definition(
                    Variable::uint(Identifier::from("n").version(2), UBitwidth::B32).into(),
                    UExpressionInner::Identifier(Identifier::from("n").version(1))
                        .annotate(UBitwidth::B32)
                        .into(),
                ),
                TypedStatement::Return(vec![FieldElementExpression::Identifier(
                    Identifier::from("a").version(2),
                )
                .into()]),
            ],
            signature: DeclarationSignature::new()
                .inputs(vec![DeclarationType::FieldElement])
                .outputs(vec![DeclarationType::FieldElement]),
        };

        let expected = TypedProgram {
            main: "main".into(),
            modules: vec![(
                "main".into(),
                TypedModule {
                    functions: vec![(
                        DeclarationFunctionKey::with_id("main").signature(
                            DeclarationSignature::new()
                                .inputs(vec![DeclarationType::FieldElement])
                                .outputs(vec![DeclarationType::FieldElement]),
                        ),
                        TypedFunctionSymbol::Here(expected_main),
                    )]
                    .into_iter()
                    .collect(),
                },
            )]
            .into_iter()
            .collect(),
        };

        assert_eq!(reduced.unwrap(), expected);
    }

    #[test]
    fn with_generics() {
        // def foo<K>(field[K] a) -> field[K]:
        //      return a
        // def main(field a) -> field:
        //      u32 n = 42
        //      n = n
        //      field[1] b = [42]
        //      b = foo(b)
        //      n = n
        //      return a

        // expected:
        // def main(field a_0) -> field:
        //      u32 n_0 = 42
        //      n_1 = n_0
        //      field[1] b_0 = [42]
        //      # PUSH CALL to foo::<1> with a_0 := b_0
        //      # POP CALL with b_1 := a_0
        //      n_2 = n_1
        //      return a_2

        let foo_signature = DeclarationSignature::new()
            .inputs(vec![DeclarationType::array(
                DeclarationType::FieldElement,
                Constant::Generic("K"),
            )])
            .outputs(vec![DeclarationType::array(
                DeclarationType::FieldElement,
                Constant::Generic("K"),
            )]);

        let foo: TypedFunction<Bn128Field> = TypedFunction {
            generics: vec!["K".into()],
            arguments: vec![DeclarationVariable::array(
                "a",
                DeclarationType::FieldElement,
                "K".into(),
            )
            .into()],
            statements: vec![TypedStatement::Return(vec![
                ArrayExpressionInner::Identifier("a".into())
                    .annotate(Type::FieldElement, 1u32)
                    .into(),
            ])],
            signature: foo_signature.clone(),
        };

        let main: TypedFunction<Bn128Field> = TypedFunction {
            generics: vec![],
            arguments: vec![DeclarationVariable::field_element("a").into()],
            statements: vec![
                TypedStatement::Definition(
                    Variable::uint("n", UBitwidth::B32).into(),
                    TypedExpression::Uint(42u32.into()),
                ),
                TypedStatement::Definition(
                    Variable::uint("n", UBitwidth::B32).into(),
                    UExpressionInner::Identifier("n".into())
                        .annotate(UBitwidth::B32)
                        .into(),
                ),
                TypedStatement::Definition(
                    Variable::array("b", Type::FieldElement, 1u32.into()).into(),
                    ArrayExpressionInner::Value(vec![
                        FieldElementExpression::Number(1.into()).into()
                    ])
                    .annotate(Type::FieldElement, 1u32)
                    .into(),
                ),
                TypedStatement::MultipleDefinition(
                    vec![Variable::array("b", Type::FieldElement, 1u32.into()).into()],
                    TypedExpressionList::FunctionCall(
                        DeclarationFunctionKey::with_id("foo").signature(foo_signature.clone()),
                        vec![ArrayExpressionInner::Identifier("b".into())
                            .annotate(Type::FieldElement, 1u32)
                            .into()],
                        vec![Type::array(Type::FieldElement, 1u32)],
                    ),
                ),
                TypedStatement::Definition(
                    Variable::uint("n", UBitwidth::B32).into(),
                    UExpressionInner::Identifier("n".into())
                        .annotate(UBitwidth::B32)
                        .into(),
                ),
                TypedStatement::Return(vec![FieldElementExpression::Identifier("a".into()).into()]),
            ],
            signature: DeclarationSignature::new()
                .inputs(vec![DeclarationType::FieldElement])
                .outputs(vec![DeclarationType::FieldElement]),
        };

        let p = TypedProgram {
            main: "main".into(),
            modules: vec![(
                "main".into(),
                TypedModule {
                    functions: vec![
                        (
                            DeclarationFunctionKey::with_id("foo").signature(foo_signature.clone()),
                            TypedFunctionSymbol::Here(foo),
                        ),
                        (
                            DeclarationFunctionKey::with_id("main").signature(
                                DeclarationSignature::new()
                                    .inputs(vec![DeclarationType::FieldElement])
                                    .outputs(vec![DeclarationType::FieldElement]),
                            ),
                            TypedFunctionSymbol::Here(main),
                        ),
                    ]
                    .into_iter()
                    .collect(),
                },
            )]
            .into_iter()
            .collect(),
        };

        let reduced = reduce_program(p);

        let expected_main = TypedFunction {
            generics: vec![],
            arguments: vec![DeclarationVariable::field_element("a").into()],
            statements: vec![
                TypedStatement::Definition(
                    Variable::uint("n", UBitwidth::B32).into(),
                    TypedExpression::Uint(42u32.into()),
                ),
                TypedStatement::Definition(
                    Variable::uint(Identifier::from("n").version(1), UBitwidth::B32).into(),
                    UExpressionInner::Identifier("n".into())
                        .annotate(UBitwidth::B32)
                        .into(),
                ),
                TypedStatement::Definition(
                    Variable::array("b", Type::FieldElement, 1u32.into()).into(),
                    ArrayExpressionInner::Value(vec![
                        FieldElementExpression::Number(1.into()).into()
                    ])
                    .annotate(Type::FieldElement, 1u32)
                    .into(),
                ),
                TypedStatement::PushCallLog(
                    "main".into(),
                    DeclarationFunctionKey::with_id("foo").signature(foo_signature.clone()),
                    vec![1u32],
                    vec![(
                        ConcreteVariable::array("a", ConcreteType::FieldElement, 1).into(),
                        ArrayExpressionInner::Identifier("b".into())
                            .annotate(Type::FieldElement, 1u32)
                            .into(),
                    )],
                ),
                TypedStatement::PopCallLog(vec![(
                    ConcreteVariable::array(
                        Identifier::from("b").version(1),
                        ConcreteType::FieldElement,
                        1,
                    ),
                    ArrayExpressionInner::Identifier("a".into())
                        .annotate(Type::FieldElement, 1u32)
                        .into(),
                )]),
                TypedStatement::Definition(
                    Variable::uint(Identifier::from("n").version(2), UBitwidth::B32).into(),
                    UExpressionInner::Identifier(Identifier::from("n").version(1))
                        .annotate(UBitwidth::B32)
                        .into(),
                ),
                TypedStatement::Return(vec![FieldElementExpression::Identifier("a".into()).into()]),
            ],
            signature: DeclarationSignature::new()
                .inputs(vec![DeclarationType::FieldElement])
                .outputs(vec![DeclarationType::FieldElement]),
        };

        let expected = TypedProgram {
            main: "main".into(),
            modules: vec![(
                "main".into(),
                TypedModule {
                    functions: vec![(
                        DeclarationFunctionKey::with_id("main").signature(
                            DeclarationSignature::new()
                                .inputs(vec![DeclarationType::FieldElement])
                                .outputs(vec![DeclarationType::FieldElement]),
                        ),
                        TypedFunctionSymbol::Here(expected_main),
                    )]
                    .into_iter()
                    .collect(),
                },
            )]
            .into_iter()
            .collect(),
        };

        assert_eq!(reduced.unwrap(), expected);
    }

    #[test]
    fn with_generics_and_propagation() {
        // def foo<K>(field[K] a) -> field[K]:
        //      return a
        // def main(field a) -> field:
        //      u32 n = 2
        //      n = n
        //      field[n - 1] b = [42]
        //      b = foo(b)
        //      n = n
        //      return a

        // expected:
        // def main(field a_0) -> field:
        //      u32 n_0 = 2
        //      n_1 = n_0
        //      field[1] b_0 = [42]
        //      # PUSH CALL to foo::<1> with a_0 := b_0
        //      # POP CALL with b_1 := a_0
        //      n_2 = n_1
        //      return a_2

        let foo_signature = DeclarationSignature::new()
            .inputs(vec![DeclarationType::array(
                DeclarationType::FieldElement,
                Constant::Generic("K"),
            )])
            .outputs(vec![DeclarationType::array(
                DeclarationType::FieldElement,
                Constant::Generic("K"),
            )]);

        let foo: TypedFunction<Bn128Field> = TypedFunction {
            generics: vec!["K".into()],
            arguments: vec![DeclarationVariable::array(
                "a",
                DeclarationType::FieldElement,
                "K".into(),
            )
            .into()],
            statements: vec![TypedStatement::Return(vec![
                ArrayExpressionInner::Identifier("a".into())
                    .annotate(Type::FieldElement, 1u32)
                    .into(),
            ])],
            signature: foo_signature.clone(),
        };

        let main: TypedFunction<Bn128Field> = TypedFunction {
            generics: vec![],
            arguments: vec![DeclarationVariable::field_element("a").into()],
            statements: vec![
                TypedStatement::Definition(
                    Variable::uint("n", UBitwidth::B32).into(),
                    TypedExpression::Uint(2u32.into()),
                ),
                TypedStatement::Definition(
                    Variable::uint("n", UBitwidth::B32).into(),
                    UExpressionInner::Identifier("n".into())
                        .annotate(UBitwidth::B32)
                        .into(),
                ),
                TypedStatement::Definition(
                    Variable::array(
                        "b",
                        Type::FieldElement,
                        UExpressionInner::Sub(
                            box UExpressionInner::Identifier("n".into()).annotate(UBitwidth::B32),
                            box 1u32.into(),
                        )
                        .annotate(UBitwidth::B32),
                    )
                    .into(),
                    ArrayExpressionInner::Value(vec![
                        FieldElementExpression::Number(1.into()).into()
                    ])
                    .annotate(Type::FieldElement, 1u32)
                    .into(),
                ),
                TypedStatement::MultipleDefinition(
                    vec![Variable::array(
                        "b",
                        Type::FieldElement,
                        UExpressionInner::Sub(
                            box UExpressionInner::Identifier("n".into()).annotate(UBitwidth::B32),
                            box 1u32.into(),
                        )
                        .annotate(UBitwidth::B32),
                    )
                    .into()],
                    TypedExpressionList::FunctionCall(
                        DeclarationFunctionKey::with_id("foo").signature(foo_signature.clone()),
                        vec![ArrayExpressionInner::Identifier("b".into())
                            .annotate(
                                Type::FieldElement,
                                UExpressionInner::Sub(
                                    box UExpressionInner::Identifier("n".into())
                                        .annotate(UBitwidth::B32),
                                    box 1u32.into(),
                                )
                                .annotate(UBitwidth::B32),
                            )
                            .into()],
                        vec![Type::array(
                            Type::FieldElement,
                            UExpressionInner::Sub(
                                box UExpressionInner::Identifier("n".into())
                                    .annotate(UBitwidth::B32),
                                box 1u32.into(),
                            )
                            .annotate(UBitwidth::B32),
                        )],
                    ),
                ),
                TypedStatement::Definition(
                    Variable::uint("n", UBitwidth::B32).into(),
                    UExpressionInner::Identifier("n".into())
                        .annotate(UBitwidth::B32)
                        .into(),
                ),
                TypedStatement::Return(vec![FieldElementExpression::Identifier("a".into()).into()]),
            ],
            signature: DeclarationSignature::new()
                .inputs(vec![DeclarationType::FieldElement])
                .outputs(vec![DeclarationType::FieldElement]),
        };

        let p = TypedProgram {
            main: "main".into(),
            modules: vec![(
                "main".into(),
                TypedModule {
                    functions: vec![
                        (
                            DeclarationFunctionKey::with_id("foo").signature(foo_signature.clone()),
                            TypedFunctionSymbol::Here(foo),
                        ),
                        (
                            DeclarationFunctionKey::with_id("main").signature(
                                DeclarationSignature::new()
                                    .inputs(vec![DeclarationType::FieldElement])
                                    .outputs(vec![DeclarationType::FieldElement]),
                            ),
                            TypedFunctionSymbol::Here(main),
                        ),
                    ]
                    .into_iter()
                    .collect(),
                },
            )]
            .into_iter()
            .collect(),
        };

        let reduced = reduce_program(p);

        let expected_main = TypedFunction {
            generics: vec![],
            arguments: vec![DeclarationVariable::field_element("a").into()],
            statements: vec![
                TypedStatement::Definition(
                    Variable::uint("n", UBitwidth::B32).into(),
                    TypedExpression::Uint(2u32.into()),
                ),
                TypedStatement::Definition(
                    Variable::uint(Identifier::from("n").version(1), UBitwidth::B32).into(),
                    UExpressionInner::Identifier("n".into())
                        .annotate(UBitwidth::B32)
                        .into(),
                ),
                TypedStatement::Definition(
                    Variable::array("b", Type::FieldElement, 1u32.into()).into(),
                    ArrayExpressionInner::Value(vec![
                        FieldElementExpression::Number(1.into()).into()
                    ])
                    .annotate(Type::FieldElement, 1u32)
                    .into(),
                ),
                TypedStatement::PushCallLog(
                    "main".into(),
                    DeclarationFunctionKey::with_id("foo").signature(foo_signature.clone()),
                    vec![1u32],
                    vec![(
                        ConcreteVariable::array("a", ConcreteType::FieldElement, 1).into(),
                        ArrayExpressionInner::Identifier("b".into())
                            .annotate(Type::FieldElement, 1u32)
                            .into(),
                    )],
                ),
                TypedStatement::PopCallLog(vec![(
                    ConcreteVariable::array(
                        Identifier::from("b").version(1),
                        ConcreteType::FieldElement,
                        1,
                    ),
                    ArrayExpressionInner::Identifier("a".into())
                        .annotate(Type::FieldElement, 1u32)
                        .into(),
                )]),
                TypedStatement::Definition(
                    Variable::uint(Identifier::from("n").version(2), UBitwidth::B32).into(),
                    UExpressionInner::Identifier(Identifier::from("n").version(1))
                        .annotate(UBitwidth::B32)
                        .into(),
                ),
                TypedStatement::Return(vec![FieldElementExpression::Identifier("a".into()).into()]),
            ],
            signature: DeclarationSignature::new()
                .inputs(vec![DeclarationType::FieldElement])
                .outputs(vec![DeclarationType::FieldElement]),
        };

        let expected = TypedProgram {
            main: "main".into(),
            modules: vec![(
                "main".into(),
                TypedModule {
                    functions: vec![(
                        DeclarationFunctionKey::with_id("main").signature(
                            DeclarationSignature::new()
                                .inputs(vec![DeclarationType::FieldElement])
                                .outputs(vec![DeclarationType::FieldElement]),
                        ),
                        TypedFunctionSymbol::Here(expected_main),
                    )]
                    .into_iter()
                    .collect(),
                },
            )]
            .into_iter()
            .collect(),
        };

        assert_eq!(reduced.unwrap(), expected);
    }
}