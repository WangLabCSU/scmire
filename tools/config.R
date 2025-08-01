args <- commandArgs(TRUE) # configure-args

# check the packages MSRV first
source("tools/msrv.R")

# Configure arguments
input_profile <- args[[1L]]
input_features <- args[[2L]]

if (!nzchar(input_profile)) {
  input_profile <- Sys.getenv("mire_PROFILE")
}

if (!nzchar(input_features)) {
  input_features <- Sys.getenv("mire_FEATURES")
}

# check DEBUG and NOT_CRAN environment variables
env_debug <- Sys.getenv("DEBUG")
env_not_cran <- Sys.getenv("NOT_CRAN")

# check if the vendored zip file exists
vendor_exists <- file.exists("src/rust/vendor.tar.xz")

is_not_cran <- env_not_cran != ""
is_debug <- env_debug != ""

if (is_debug) {
  # if we have DEBUG then we set not cran to true
  # CRAN is always release build
  is_not_cran <- TRUE
  message("Creating DEBUG build.")
}

if (!is_not_cran) {
  message("Building for CRAN.")
}

# we set cran flags only if NOT_CRAN is empty and if
# the vendored crates are present.
.cran_flags <- ifelse(
  !is_not_cran && vendor_exists,
  "-j 2 --offline",
  ""
)

# when DEBUG env var is present we use `--debug` build
.profile <- if (nzchar(input_profile)) {
  message("Using input profile: --", input_profile)
  sprintf("--%s", input_profile)
} else if (is_debug) {
  ""
} else {
  "--release"
}

if (nzchar(input_features)) {
  .features <- unique(strsplit(input_features, "[, ]")[[1L]])
  .features <- .features[nzchar(.features)]
  if (length(.features)) {
    # Space or comma separated list of features to activate
    .features <- paste(.features, collapse = ",")
    message("Using input features: ", .features)
    .features <- sprintf("--features %s", .features)
  }
} else {
  .features <- ""
}

.clean_targets <- ifelse(is_debug, "", "$(TARGET_DIR)")

# when we are using a debug build we need to use target/debug instead of target/release
.libdir <- ifelse(is_debug, "debug", "release")

# read in the Makevars.in file
is_windows <- .Platform[["OS.type"]] == "windows"

# if windows we replace in the Makevars.win.in
mv_fp <- ifelse(
  is_windows,
  "src/Makevars.win.in",
  "src/Makevars.in"
)

# set the output file
mv_ofp <- ifelse(
  is_windows,
  "src/Makevars.win",
  "src/Makevars"
)

# delete the existing Makevars{.win}
if (file.exists(mv_ofp)) {
  message("Cleaning previous `", mv_ofp, "`.")
  invisible(file.remove(mv_ofp))
}

# read as a single string
mv_txt <- readLines(mv_fp)

# replace placeholder values
new_txt <- gsub("@CRAN_FLAGS@", .cran_flags, mv_txt) |>
  gsub("@PROFILE@", .profile, x = _) |>
  gsub("@CLEAN_TARGET@", .clean_targets, x = _) |>
  gsub("@LIBDIR@", .libdir, x = _) |>
  gsub("@FEATURES@", .features, x = _)

message("Writing `", mv_ofp, "`.")
con <- file(mv_ofp, open = "wb")
writeLines(new_txt, con, sep = "\n")
close(con)

message("`tools/config.R` has finished.")
