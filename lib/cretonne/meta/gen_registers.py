"""
Generate register bank descriptions for each ISA.
"""

from __future__ import absolute_import
import srcgen

try:
    from typing import Sequence, List  # noqa
    from cdsl.isa import TargetISA  # noqa
    from cdsl.registers import RegBank, RegClass  # noqa
except ImportError:
    pass


def gen_regbank(regbank, fmt):
    # type: (RegBank, srcgen.Formatter) -> None
    """
    Emit a static data definition for regbank.
    """
    with fmt.indented('RegBank {', '},'):
        fmt.format('name: "{}",', regbank.name)
        fmt.format('first_unit: {},', regbank.first_unit)
        fmt.format('units: {},', regbank.units)
        fmt.format(
                'names: &[{}],',
                ', '.join('"{}"'.format(n) for n in regbank.names))
        fmt.format('prefix: "{}",', regbank.prefix)
        fmt.format('first_toprc: {},', regbank.toprcs[0].index)
        fmt.format('num_toprcs: {},', len(regbank.toprcs))


def gen_regclass(rc, fmt):
    # type: (RegClass, srcgen.Formatter) -> None
    """
    Emit a static data definition for a register class.
    """
    with fmt.indented('RegClassData {', '},'):
        fmt.format('name: "{}",', rc.name)
        fmt.format('index: {},', rc.index)
        fmt.format('width: {},', rc.width)
        fmt.format('bank: {},', rc.bank.index)
        fmt.format('toprc: {},', rc.toprc.index)
        fmt.format('first: {},', rc.bank.first_unit + rc.start)
        fmt.format('subclasses: 0x{:x},', rc.subclass_mask())
        mask = ', '.join('0x{:08x}'.format(x) for x in rc.mask())
        fmt.format('mask: [{}],', mask)


def gen_isa(isa, fmt):
    # type: (TargetISA, srcgen.Formatter) -> None
    """
    Generate register tables for isa.
    """
    if not isa.regbanks:
        print('cargo:warning={} has no register banks'.format(isa.name))

    with fmt.indented('pub static INFO: RegInfo = RegInfo {', '};'):
        # Bank descriptors.
        with fmt.indented('banks: &[', '],'):
            for regbank in isa.regbanks:
                gen_regbank(regbank, fmt)
        fmt.line('classes: &CLASSES,')

    # Register class descriptors.
    with fmt.indented(
            'const CLASSES: [RegClassData; {}] = ['
            .format(len(isa.regclasses)), '];'):
        for idx, rc in enumerate(isa.regclasses):
            assert idx == rc.index
            gen_regclass(rc, fmt)

    # Emit constants referencing the register classes.
    for rc in isa.regclasses:
        fmt.line('#[allow(dead_code)]')
        fmt.line(
                'pub const {}: RegClass = &CLASSES[{}];'
                .format(rc.name, rc.index))


def generate(isas, out_dir):
    # type: (Sequence[TargetISA], str) -> None
    for isa in isas:
        fmt = srcgen.Formatter()
        gen_isa(isa, fmt)
        fmt.update_file('registers-{}.rs'.format(isa.name), out_dir)
