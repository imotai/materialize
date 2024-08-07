# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

from materialize.output_consistency.data_type.data_type_with_values import (
    DataTypeWithValues,
)
from materialize.output_consistency.expression.expression_characteristics import (
    ExpressionCharacteristics,
)
from materialize.output_consistency.input_data.types.number_types_provider import (
    NUMERIC_DATA_TYPES,
    NumberDataType,
)

VALUES_PER_NUMERIC_DATA_TYPE: dict[NumberDataType, DataTypeWithValues] = dict()

for num_data_type in NUMERIC_DATA_TYPES:
    values_of_type = DataTypeWithValues(num_data_type)
    VALUES_PER_NUMERIC_DATA_TYPE[num_data_type] = values_of_type

    values_of_type.add_raw_value("0", "ZERO", {ExpressionCharacteristics.ZERO})
    values_of_type.add_raw_value(
        "1",
        "ONE",
        {
            ExpressionCharacteristics.ONE,
            ExpressionCharacteristics.TINY_VALUE,
            ExpressionCharacteristics.NON_EMPTY,
        },
    )
    values_of_type.add_raw_value(
        num_data_type.max_value,
        "MAX",
        {ExpressionCharacteristics.MAX_VALUE, ExpressionCharacteristics.NON_EMPTY},
    )

    if num_data_type.is_signed and num_data_type.max_negative_value is not None:
        values_of_type.add_raw_value(
            f"{num_data_type.max_negative_value}",
            "NEG_MAX",
            {
                ExpressionCharacteristics.NEGATIVE,
                ExpressionCharacteristics.MAX_VALUE,
                ExpressionCharacteristics.NON_EMPTY,
            },
        )

    if num_data_type.is_decimal:
        # only add this value for decimal types because 1 is always added
        values_of_type.add_raw_value(
            num_data_type.smallest_value,
            "TINY",
            {
                ExpressionCharacteristics.TINY_VALUE,
                ExpressionCharacteristics.NON_EMPTY,
                ExpressionCharacteristics.DECIMAL,
            },
        )

        # also only for decimal types
        values_of_type.add_raw_value(
            "'NaN'",
            "NAN",
            {
                ExpressionCharacteristics.NAN,
                ExpressionCharacteristics.DECIMAL,
            },
        )

    if num_data_type.supports_infinity:
        values_of_type.add_raw_value(
            "'+Infinity'",
            "P_INFINITY",
            {
                ExpressionCharacteristics.INFINITY,
            },
        )
        values_of_type.add_raw_value(
            "'-Infinity'",
            "N_INFINITY",
            {
                ExpressionCharacteristics.INFINITY,
            },
        )

    for index, tiny_value in enumerate(num_data_type.further_tiny_dec_values):
        values_of_type.add_raw_value(
            tiny_value,
            f"TINY{index + 2}",
            {
                ExpressionCharacteristics.TINY_VALUE,
                ExpressionCharacteristics.NON_EMPTY,
                ExpressionCharacteristics.DECIMAL,
            },
        )

for type_definition, values_of_type in VALUES_PER_NUMERIC_DATA_TYPE.items():
    for value in values_of_type.raw_values:
        if ExpressionCharacteristics.MAX_VALUE in value.own_characteristics:
            value.own_characteristics.add(ExpressionCharacteristics.LARGE_VALUE)
