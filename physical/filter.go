package physical

import (
	"context"

	"github.com/cube2222/octosql/execution"
	"github.com/pkg/errors"
)

type Filter struct {
	Formula Formula
	Source  Node
}

func NewFilter(formula Formula, child Node) *Filter {
	return &Filter{Formula: formula, Source: child}
}

func (node *Filter) Transform(ctx context.Context, transformers *Transformers) Node {
	var transformed Node = &Filter{
		Formula: node.Formula.Transform(ctx, transformers),
		Source:  node.Source.Transform(ctx, transformers),
	}
	if transformers.NodeT != nil {
		transformed = transformers.NodeT(transformed)
	}
	return transformed
}

func (node *Filter) Materialize(ctx context.Context, matCtx *MaterializationContext) (execution.Node, error) {
	materializedFormula, err := node.Formula.Materialize(ctx, matCtx)
	if err != nil {
		return nil, errors.Wrap(err, "couldn't materialize formula")
	}
	materializedSource, err := node.Source.Materialize(ctx, matCtx)
	if err != nil {
		return nil, errors.Wrap(err, "couldn't materialize Source")
	}
	return execution.NewFilter(materializedFormula, materializedSource), nil
}
