package=native_fmt
$(package)_version=11.2.0
$(package)_download_path=https://github.com/fmtlib/fmt/archive/refs/tags
$(package)_download_file=$($(package)_version).tar.gz
$(package)_file_name=fmt-$($(package)_version).tar.gz
$(package)_sha256_hash=bc23066d87ab3168f27cef3e97d545fa63314f5c79df5ea444d41d56f962c6af
$(package)_build_subdir=build
$(package)_dependencies=native_cmake

define $(package)_set_vars
$(package)_config_opts += -DCMAKE_BUILD_TYPE=Release
$(package)_config_opts += -DCMAKE_POSITION_INDEPENDENT_CODE=TRUE
$(package)_config_opts += -DFMT_TEST=FALSE
endef

define $(package)_preprocess_cmds
  mkdir $($(package)_build_subdir)
endef

define $(package)_config_cmds
  $($(package)_cmake) .. $($(package)_config_opts)
endef

define $(package)_build_cmds
  $(MAKE)
endef

define $(package)_stage_cmds
  $(MAKE) DESTDIR=$($(package)_staging_dir) install
endef
