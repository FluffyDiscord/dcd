<?php

/*
 * Dumps the two data providers of the vendored `DotenvTest.php` to JSON, so the
 * Rust port runs UPSTREAM'S OWN CASES instead of a hand-transcription of them
 * (spec §10.1 TC-031). The JSON is committed; this script only has to run when
 * the vendored copy is refreshed:
 *
 *   docker run --rm -v "$PWD/tests/fixtures/symfony:/work" -w /work php:8.3-cli \
 *     php extract_cases.php > dotenv_cases.json
 *
 * PHPUnit is not installed and not wanted: the providers are plain static
 * methods, so a stub base class is all it takes to include the file. The
 * attributes are never reflected, so their classes are never instantiated.
 */

namespace PHPUnit\Framework {
    class TestCase
    {
    }
}

namespace PHPUnit\Framework\Attributes {
    #[\Attribute]
    class DataProvider
    {
        public function __construct(...$arguments)
        {
        }
    }

    #[\Attribute]
    class TestWith
    {
        public function __construct(...$arguments)
        {
        }
    }
}

namespace {
    require __DIR__.'/DotenvTest.php';

    $testClass = \Symfony\Component\Dotenv\Tests\DotenvTest::class;

    $values = [];
    foreach ($testClass::getEnvData() as [$input, $expected]) {
        $pairs = [];
        foreach ($expected as $key => $value) {
            $pairs[] = [(string) $key, $value];
        }
        $values[] = ['input' => $input, 'expected' => $pairs];
    }

    $errors = [];
    foreach ($testClass::getEnvDataWithFormatErrors() as [$input, $message]) {
        $errors[] = ['input' => $input, 'message' => $message];
    }

    $document = [
        'source' => 'symfony/dotenv 8.1 — Tests/DotenvTest.php, providers getEnvData + getEnvDataWithFormatErrors',
        'generated_by' => 'tests/fixtures/symfony/extract_cases.php',
        'values' => $values,
        'errors' => $errors,
    ];

    echo json_encode($document, JSON_PRETTY_PRINT | JSON_UNESCAPED_SLASHES), "\n";
}
