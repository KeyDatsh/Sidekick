# The cmake crate replaces CMake's Release flags and drops /O2 on the way (Visual Studio generator),
# which leaves all the C and C++ code unoptimised. Put the optimisation back, Release only.
foreach(lang C CXX)
  if(NOT "$CACHE{CMAKE_${lang}_FLAGS_RELEASE}" MATCHES "/O2")
    set(CMAKE_${lang}_FLAGS_RELEASE "$CACHE{CMAKE_${lang}_FLAGS_RELEASE} /O2 /Ob2 /DNDEBUG" CACHE STRING "" FORCE)
  endif()
endforeach()
