// Derived from TiDB v8.5.8 pkg/expression/builtin_vec_vec_test.go (Apache License 2.0).

// The Go side of the N4 §5 gate for the vector distance functions (D08.1 T7a), copied
// into pkg/expression of TiDB v8.5.8 by scripts/eval-gate.sh. Go's own
// BenchmarkVectorizedBuiltinVecFunc only has 3-dimensional inputs (plus NULL cases);
// these cases use the same generator at 3 and 768 dimensions, one case per function.
// Each case reports the vector version (-VecBuiltinFunc) and the row version
// (-NonVecBuiltinFunc), 1024 rows per batch.

package expression

import (
	"testing"

	"github.com/pingcap/tidb/pkg/parser/ast"
	"github.com/pingcap/tidb/pkg/types"
)

func gateVecCases(dim int) vecExprBenchCases {
	cases := vecExprBenchCases{}
	for _, f := range []string{ast.VecL1Distance, ast.VecL2Distance, ast.VecCosineDistance, ast.VecNegativeInnerProduct} {
		cases[f] = []vecExprBenchCase{{
			retEvalType:   types.ETReal,
			childrenTypes: []types.EvalType{types.ETVectorFloat32, types.ETVectorFloat32},
			geners:        []dataGenerator{newVectorFloat32RandGener(dim), newVectorFloat32RandGener(dim)},
		}}
	}
	return cases
}

func BenchmarkGateVecDim3(b *testing.B) { benchmarkVectorizedBuiltinFunc(b, gateVecCases(3)) }

func BenchmarkGateVecDim768(b *testing.B) { benchmarkVectorizedBuiltinFunc(b, gateVecCases(768)) }
