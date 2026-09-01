plugins {
    kotlin("jvm") version "1.9.24"
}

repositories {
    mavenCentral()
}

dependencies {
    implementation("net.java.dev.jna:jna:5.17.0")
    implementation("org.jetbrains.kotlinx:kotlinx-serialization-json:1.6.3")
    testImplementation(kotlin("test-junit5"))
    testRuntimeOnly("org.junit.jupiter:junit-jupiter-engine:5.10.2")
}

tasks.withType<org.jetbrains.kotlin.gradle.tasks.KotlinCompile>().configureEach {
    kotlinOptions.jvmTarget = "1.8"
}

@Suppress("DEPRECATION")
val hostOs = System.getProperty("os.name").toLowerCase()
@Suppress("DEPRECATION")
val hostArch = System.getProperty("os.arch").toLowerCase()
val fakeLibraryName = when {
    hostOs.contains("mac") -> "libfake_rings.dylib"
    hostOs.contains("win") -> "fake_rings.dll"
    else -> "libfake_rings.so"
}
val fakeLibrary = layout.buildDirectory.file("native/$fakeLibraryName")
val actualLibrary = providers.environmentVariable("RINGS_FFI_LIBRARY")
val requireActualLibrary = providers.environmentVariable("RINGS_FFI_REQUIRE_LIBRARY")

val buildFakeLibrary by tasks.registering(Exec::class) {
    val output = fakeLibrary.get().asFile
    inputs.file(projectDir.resolve("../tests/fake_rings.c"))
    outputs.file(output)
    doFirst { output.parentFile.mkdirs() }
    val compilerArguments = mutableListOf("cc")
    when {
        hostOs.contains("mac") -> {
            compilerArguments += "-dynamiclib"
            compilerArguments += listOf(
                "-arch",
                if (hostArch == "aarch64") "arm64" else hostArch,
            )
        }
        hostOs.contains("win") -> compilerArguments += "-shared"
        else -> compilerArguments += listOf("-shared", "-fPIC")
    }
    compilerArguments += listOf(
        projectDir.resolve("../tests/fake_rings.c").absolutePath,
        "-o",
        output.absolutePath,
    )
    commandLine(compilerArguments)
}

tasks.test {
    dependsOn(buildFakeLibrary)
    useJUnitPlatform()
    doFirst {
        systemProperty("rings.fake.library", fakeLibrary.get().asFile.absolutePath)
        actualLibrary.orNull?.let { systemProperty("rings.actual.library", it) }
        systemProperty("rings.require.actual.library", requireActualLibrary.orNull ?: "0")
    }
}
