# Copyright 2019 DTAI Research Group - KU Leuven.
# License: Apache License 2.0
# Author: Laurens Devos

import os
import csv
import textwrap

from ctypes import *

import numpy as np


CONFIG_CSV = "bitboost_config.gen.csv"

def _get_lib_path():
    d = os.path.dirname(__file__)
    in_source_debug_so = os.path.join(d, "../../target/debug/libbitboost.so")
    in_source_release_so = os.path.join(d, "../../target/release/libbitboost.so")
    installed_so = os.path.join(d, "libbitboost.so")

    if os.path.isfile(installed_so):
        return installed_so

    d_isfile = os.path.isfile(in_source_debug_so)
    r_isfile = os.path.isfile(in_source_release_so)
    if d_isfile and r_isfile:
        # use debug if newer, warn
        if os.path.getmtime(in_source_debug_so) > os.path.getmtime(in_source_release_so):
            print("WARNING: using newer BitBoost debug build")
            return in_source_debug_so
        else:
            return in_source_release_so
    elif d_isfile:
        return in_source_debug_so
    elif r_isfile:
        return in_source_release_so

    raise Exception("BitBoost library could not be located")

def _get_config_csv():
    d = os.path.dirname(__file__)
    in_source_config = os.path.join(d, "../", CONFIG_CSV)
    installed_config = os.path.join(d, CONFIG_CSV)

    if os.path.isfile(installed_config):
        return installed_config
    elif os.path.isfile(in_source_config):
        return in_source_config

    raise Exception("BitBoost config_csv not found")

class RawBitBoost:
    _lib = CDLL(_get_lib_path())
    print(_lib)
    _rust_numt_nbytes = _lib.bb_get_numt_nbytes
    _rust_numt_nbytes.argtypes = []
    _rust_numt_nbytes.restype = c_int
    _numt_nbytes = _rust_numt_nbytes()

    numt = c_double if _numt_nbytes == 8 else c_float
    numt_p = POINTER(numt)
    numt.__doc__ = "BitBoost float type."
    numt_p.__doc__ = "BitBoost float pointer type."

    _rust_alloc = _lib.bb_alloc
    _rust_alloc.argtypes = [c_int]
    _rust_alloc.restype = c_void_p

    _rust_dealloc = _lib.bb_dealloc
    _rust_dealloc.argtypes = [c_void_p]
    _rust_dealloc.restype = c_int

    _rust_refresh_data = _lib.bb_refresh_data
    _rust_refresh_data.argtypes = [c_void_p, c_int]
    _rust_refresh_data.restype = c_int

    _rust_set_fdata = _lib.bb_set_feature_data
    _rust_set_fdata.argtypes = [c_void_p, c_int, numt_p, c_int]
    _rust_set_fdata.restype = c_int

    _rust_set_config_field = _lib.bb_set_config_field
    _rust_set_config_field.argtypes = [c_void_p, c_char_p, c_char_p]
    _rust_set_config_field.restype = c_int

    _rust_train = _lib.bb_train
    _rust_train.argtypes = [c_void_p]
    _rust_train.restype = c_int

    _rust_predict = _lib.bb_predict
    _rust_predict.argtypes = [c_void_p, numt_p]
    _rust_predict.restype = c_int

    def __init__(self, nfeatures, nexamples):
        assert nfeatures > 0
        assert nexamples > 0
        self._nfeatures = nfeatures
        self._nexamples = -1 # set by set_data
        self._ctx_ptr = self._rust_alloc(self._nfeatures)

    def __del__(self):
        if self._ctx_ptr:
            self.dealloc()

    def dealloc(self):
        self._check()
        self._rust_dealloc(self._ctx_ptr)
        self._ctx_ptr = c_void_p(0)

    def _check(self):
        if not self._ctx_ptr:
            raise Exception("no BitBoost context")

    def set_feature_data(self, feat_id, data, is_categorical):
        self._check()
        assert isinstance(data, np.ndarray)
        assert data.dtype == self.numt
        assert 0 <= feat_id and feat_id <= self._nfeatures
        assert isinstance(is_categorical, bool) 
        data = data.copy() # make copy to ensure congiguous, not optimal
        data_ptr = data.ctypes.data_as(self.numt_p)
        is_cat = 1 if is_categorical else 0
        self._rust_set_fdata(self._ctx_ptr, feat_id, data_ptr, is_cat)

    def set_data(self, data, cat_features = set()):
        self._check()
        assert isinstance(data, np.ndarray)
        assert data.dtype == self.numt
        assert data.shape[1] == self._nfeatures

        self._nexamples = data.shape[0]
        self._rust_refresh_data(self._ctx_ptr, self._nexamples)

        for feat_id in range(self._nfeatures):
            is_cat = feat_id in cat_features
            self.set_feature_data(feat_id, data[:,feat_id], is_cat)
    
    def set_target(self, data):
        self._check()
        self.set_feature_data(self._nfeatures, data, False)

    def set_config_field(self, name, value):
        self._check()
        if self.config_params[name].type_str.startswith("Vec"):
            if not isinstance(value, str):
                value = ",".join(map(str, value))
        n = c_char_p(bytes(str(name), "utf8"))
        v = c_char_p(bytes(str(value), "utf8"))
        self._rust_set_config_field(self._ctx_ptr, n, v)

    def set_config(self, values):
        self._check()
        assert isinstance(values, dict)
        for name, value in values.items():
            self.set_config_field(name, value)

    def train(self):
        self._check()
        self._rust_train(self._ctx_ptr)

    def predict(self):
        self._check()
        assert self._nexamples > 0
        output = np.zeros(self._nexamples, dtype=self.numt)
        output_ptr = output.ctypes.data_as(self.numt_p)
        self._rust_predict(self._ctx_ptr, output_ptr)
        return output

    def write_model(self):
        raise Exception("not implemented")

    def read_model(self):
        raise Exception("not implemented")



class BitBoostConfigParam:
    def __init__(self, name, default, type_str, descr):
        self.name = name
        self.default = default
        self.type_str = type_str
        self.descr = descr

    def __str__(self):
        s = ""
        default = self.default_value_str()
        descr = "\n        ".join(textwrap.wrap(self.descr, 80))
        s += "\n    {} : {} (default {})".format(self.name, self.type_str, default)
        s += "\n        {}\n".format(descr)
        return s

    def default_value_str(self):
        if self.type_str == "String":
            return "\"{}\"".format(self.default)
        return self.default

def get_config_params():
    config_params = {}
    with open(_get_config_csv()) as f:
        r = csv.reader(f)
        next(r)
        for row in r:
            if "cli only" in row[3]:
                continue
            config_params[row[0]] = BitBoostConfigParam(row[0], row[1], row[2], row[3])
    return config_params

def gen_config_doc(config_params):
    doc = "\nPARAMETERS\n----------"
    for name, item in config_params.items():
        doc += str(item)
    return doc

def gen_init_fun(config_params, filename):
    source = "def __init__(self"
    for name, item in config_params.items():
        source += ", {}={}".format(name, item.default_value_str())
    source += "):\n"
    for name, item in config_params.items():
        source += "    self.{}={}\n".format(name, name)
    
    _locals = {}
    exec(source, None, _locals)

    initf = _locals["__init__"]
    initf.__doc__ = gen_config_doc(config_params)
    return initf

RawBitBoost.config_params = get_config_params()
