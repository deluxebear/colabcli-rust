#!/usr/bin/env python3
"""Compare global and local account selectors before offline queries and mutations."""
from pathlib import Path
from compare_merged_account_mutations import main


def scenarios():
    return [
        ('root-missing-list', [['-a','missing','account','list','--json']]),
        ('root-missing-current', [['-a','missing','account','current','--json']]),
        ('root-missing-use', [['-a','missing','account','use','work']]),
        ('env-missing-use', [['account','use','work']]),
        ('root-overrides-missing-env', [['-a','work','account','use','default']]),
        ('root-missing-rename', [['-a','missing','account','rename','work','new-work']]),
        ('root-missing-remove', [['-a','missing','account','remove','work','--force']]),
        ('root-missing-default-drive', [['-a','missing','account','set-drive','adc','--default']]),
        ('local-ignored-default-drive', [['account','set-drive','adc','--default','-a','missing']]),
        ('local-missing-bind', [['account','set-drive','adc','-a','missing']]),
        ('root-and-local-bind', [['-a','default','account','set-drive','adc','-a','work']]),
        ('root-missing-local-valid', [['-a','missing','account','set-drive','adc','-a','work']]),
        ('root-valid-local-missing', [['-a','work','account','set-drive','adc','-a','missing']]),
        ('trimmed-env-current', [['account','current','--json']]),
        ('blank-env-current', [['account','current','--json']]),
        ('empty-root-falls-back-env', [['-a','','account','current','--json']]),
        ('empty-root-falls-back-active', [['-a','','account','current','--json']]),
        ('root-email-current', [['-a','FIXTURE@example.invalid','account','current','--json']]),
        ('repeated-root-last-wins', [['-a','work','-a','adc','account','current','--json']]),
        ('option-looking-literal', [['account','use','--','--account']]),
        ('root-error-before-invalid-rename', [['-a','missing','account','rename','work','../escape']]),
    ]


def environment(name):
    return {'COLAB_ACCOUNT': {
        'env-missing-use':'missing', 'root-overrides-missing-env':'missing',
        'trimmed-env-current':' work ', 'blank-env-current':'   ',
        'empty-root-falls-back-env':'work',
    }[name]} if name in {
        'env-missing-use','root-overrides-missing-env','trimmed-env-current',
        'blank-env-current','empty-root-falls-back-env',
    } else {}


if __name__ == '__main__':
    main(case_provider=scenarios, extra_environment=environment,
         default_output=Path(__file__).resolve().parents[1] / 'research/merged-account-selection.json',
         scope='merged global/local account selection before offline account queries and mutations; no real credentials or cloud resources')
